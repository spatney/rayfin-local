//! Live reload of the built binary (`serve --json`) on a copy of the fixture app: `rayfin.yml`
//! edits applied on the same ports, new secret values restarting only the function host, and the
//! Vite dev server started again after it is killed.
//!
//! They need Node.js and the fixture's npm packages (`npm ci` in `tests/fixtures/apps/basic` and
//! in its `rayfin/functions`); without them they print a note and pass.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long a step may take: a start, or a change that rebuilds the functions.
const STEP: Duration = Duration::from_secs(240);

fn rayfin_local() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rayfin-local"))
}

fn fixture_dir() -> PathBuf {
    let dir = ["..", "..", "tests", "fixtures", "apps", "basic"]
        .iter()
        .fold(PathBuf::from(env!("CARGO_MANIFEST_DIR")), |dir, part| {
            dir.join(part)
        });
    std::path::absolute(dir).unwrap()
}

/// A copy of the fixture app in a temporary folder, its `node_modules` linked to the fixture's.
struct App {
    dir: tempfile::TempDir,
    links: Vec<PathBuf>,
}

impl App {
    /// `None` (with a note) when Node.js or the fixture's packages are missing.
    fn copy() -> Option<Self> {
        let fixture = fixture_dir();
        let installed = [
            "node_modules/@microsoft/rayfin-core",
            "node_modules/vite",
            "rayfin/functions/node_modules/@microsoft/fabric-user-data-functions",
        ]
        .iter()
        .all(|package| fixture.join(package).join("package.json").is_file());
        if !installed {
            eprintln!(
                "skipped: the fixture's packages are not installed (run `npm ci` in {0} and in \
                 {0}/rayfin/functions)",
                fixture.display()
            );
            return None;
        }
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("skipped: Node.js is not on PATH");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&fixture, dir.path());
        let mut app = Self {
            dir,
            links: Vec::new(),
        };
        for modules in [
            &["node_modules"][..],
            &["rayfin", "functions", "node_modules"],
        ] {
            let at = |root: &Path| {
                modules
                    .iter()
                    .fold(root.to_owned(), |dir, part| dir.join(part))
            };
            let link = at(app.path());
            link_dir(&at(&fixture), &link);
            app.links.push(link);
        }
        Some(app)
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn config_path(&self) -> PathBuf {
        self.path().join("rayfin").join("rayfin.yml")
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.config_path())
            .unwrap()
            .replace("\r\n", "\n")
    }

    /// Replaces each `from` (which must be there) with its `to` in `rayfin.yml`, in one write.
    fn edit_config(&self, edits: &[(&str, &str)]) {
        let mut config = self.config();
        for (from, to) in edits {
            assert!(config.contains(from), "{from:?} is not in:\n{config}");
            config = config.replacen(from, to, 1);
        }
        std::fs::write(self.config_path(), config).unwrap();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // The links go first, so that deleting the copy never reaches the fixture's packages.
        for link in &self.links {
            if std::fs::remove_dir(link).is_err() {
                let _ = std::fs::remove_file(link);
            }
        }
    }
}

/// Copies the fixture without what its runs leave (packages, state, build output).
fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(name.as_ref(), "node_modules" | ".rayfin-local" | "dist")
            || name.ends_with(".tsbuildinfo")
        {
            continue;
        }
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[cfg(windows)]
fn link_dir(target: &Path, link: &Path) {
    // A junction needs no privilege, unlike a symbolic link.
    let status = Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "could not link {}", link.display());
}

#[cfg(unix)]
fn link_dir(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// A `serve --json` that runs until stdin ends, its events read as they come.
struct Serve {
    child: Child,
    events: Receiver<Value>,
    stderr: Receiver<String>,
    seen: Vec<Value>,
}

impl Serve {
    fn start(project: &Path, args: &[&str]) -> Self {
        let mut child = rayfin_local()
            .args(["serve", "--json", "-C"])
            .arg(project)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (events_tx, events) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                let event = serde_json::from_str(&line)
                    .unwrap_or_else(|err| panic!("not a JSON line ({err}): {line}"));
                if events_tx.send(event).is_err() {
                    break;
                }
            }
        });
        let (stderr_tx, stderr) = mpsc::channel();
        let mut errors = child.stderr.take().unwrap();
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = errors.read_to_string(&mut text);
            let _ = stderr_tx.send(text);
        });
        Self {
            child,
            events,
            stderr,
            seen: Vec::new(),
        }
    }

    /// The events until one `wanted` accepts (included), which is returned.
    fn until(&mut self, what: &str, limit: Duration, wanted: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(event) = self.events.recv_timeout(left) else {
                panic!("no {what} in {limit:?}:\n{}", self.report());
            };
            self.seen.push(event.clone());
            if wanted(&event) {
                return event;
            }
            if event["type"] == "stopped" || event["fatal"] == true {
                panic!("stopped before {what}:\n{}", self.report());
            }
        }
    }

    /// The events until one of type `kind`, which is returned.
    fn next(&mut self, kind: &str) -> Value {
        self.until(&format!("`{kind}` event"), STEP, |event| {
            event["type"] == kind
        })
    }

    /// The events seen since `from` (an index in `seen`) of type `kind`.
    fn seen_since(&self, from: usize, kind: &str) -> Vec<&Value> {
        self.seen[from..]
            .iter()
            .filter(|event| event["type"] == kind)
            .collect()
    }

    /// Closes stdin, then waits for the exit.
    fn close_stdin(&mut self) -> ExitStatus {
        drop(self.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("rayfin-local did not exit:\n{}", self.report());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn report(&self) -> String {
        let events: Vec<String> = self.seen.iter().map(Value::to_string).collect();
        let stderr = self
            .stderr
            .recv_timeout(Duration::from_millis(200))
            .unwrap_or_default();
        format!("events:\n{}\nstderr:\n{stderr}", events.join("\n"))
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An HTTP/1.1 request over plain TCP: the status and the body.
fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&Value>,
) -> (u16, String) {
    let rest = url.strip_prefix("http://").unwrap();
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = TcpStream::connect(authority).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let body = body.map(Value::to_string).unwrap_or_default();
    let mut head = format!(
        "{method} /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    write!(stream, "{head}\r\n{body}").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
}

/// The app's publishable key, from `/rayfin.config.json`.
fn publishable_key(api_url: &str) -> String {
    let (status, body) = request("GET", &format!("{api_url}/rayfin.config.json"), &[], None);
    assert_eq!(status, 200, "{body}");
    let config: Value = serde_json::from_str(&body).unwrap();
    config["publishableKey"].as_str().unwrap().to_owned()
}

/// Invokes function `name` anonymously: the status and the envelope's `output`.
fn invoke(api_url: &str, key: &str, name: &str) -> (u16, Value) {
    let (status, body) = request(
        "POST",
        &format!("{api_url}/functions/{name}/invoke"),
        &[("X-Publishable-Key", key)],
        Some(&json!({})),
    );
    let envelope: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    (status, envelope["output"].clone())
}

/// Every process: its id, its parent's and its command line.
fn processes() -> Vec<(u32, u32, String)> {
    let output = if cfg!(windows) {
        let list = "Get-CimInstance Win32_Process | ForEach-Object { \
                    \"$($_.ProcessId) $($_.ParentProcessId) $($_.CommandLine)\" }";
        Command::new("powershell")
            .args(["-NoProfile", "-Command", list])
            .output()
    } else {
        Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,args="])
            .output()
    }
    .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.trim().splitn(3, char::is_whitespace);
            let pid = parts.next()?.parse().ok()?;
            let parent = parts.next()?.parse().ok()?;
            Some((pid, parent, parts.next().unwrap_or_default().to_owned()))
        })
        .collect()
}

/// The processes `pid` started (and those they started) whose command line contains `text`.
fn started_by(pid: u32, text: &str) -> Vec<u32> {
    let all = processes();
    let mut found = Vec::new();
    let mut parents = vec![pid];
    while let Some(parent) = parents.pop() {
        for (child, of, command) in &all {
            if *of == parent && *child != parent {
                parents.push(*child);
                if command.contains(text) {
                    found.push(*child);
                }
            }
        }
    }
    found
}

/// The one process `pid` started whose command line contains `text`, once there is just one
/// other than `previous` (a replaced process may take a moment to exit).
fn replaced(pid: u32, text: &str, previous: u32) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let found = started_by(pid, text);
        match found.as_slice() {
            [one] if *one != previous => return *one,
            _ if Instant::now() > deadline => {
                panic!("expected one `{text}` process other than {previous}: {found:?}")
            }
            _ => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// Kills process `pid` (and the processes it started) at once.
fn kill(pid: u32) {
    let status = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .status()
    } else {
        Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
    }
    .unwrap();
    assert!(status.success(), "could not kill {pid}");
}

#[test]
fn rayfin_yml_changes_apply_on_the_same_ports() {
    let Some(app) = App::copy() else {
        return;
    };
    let mut serve = Serve::start(app.path(), &["--no-frontend", "--port", "0"]);
    let ready = serve.next("ready");
    let api_url = ready["apiUrl"].as_str().unwrap().to_owned();
    let features = &ready["features"];
    assert_eq!(features["storage"]["enabled"], true, "{ready}");
    assert_eq!(features["auth"]["password"], true, "{ready}");
    let key = publishable_key(&api_url);
    let storage_list = format!("{api_url}/api/storage/list");
    let key_header = [("X-Publishable-Key", key.as_str())];
    assert_ne!(request("GET", &storage_list, &key_header, None).0, 404);
    assert_eq!(invoke(&api_url, &key, "ping"), (200, json!("pong")));

    app.edit_config(&[(
        "  storage:\n    enabled: true",
        "  storage:\n    enabled: false",
    )]);
    let changed = serve.next("config-changed");
    assert_eq!(changed["applied"], true, "{changed}");
    assert_eq!(changed["restartRequired"], false, "{changed}");
    assert_eq!(changed["what"], json!(["storage"]), "{changed}");
    let ready = serve.next("ready");
    assert_eq!(ready["apiUrl"], api_url.as_str());
    assert_eq!(ready["features"]["storage"]["enabled"], false, "{ready}");
    assert_eq!(request("GET", &storage_list, &key_header, None).0, 404);

    // The data API goes, then comes back over the same tables.
    let graphql = |api_url: &str| {
        let query = json!({ "query": "{ __typename }" });
        request(
            "POST",
            &format!("{api_url}/graphql"),
            &key_header,
            Some(&query),
        )
        .0
    };
    assert_eq!(graphql(&api_url), 200);
    app.edit_config(&[("  data:\n    enabled: true", "  data:\n    enabled: false")]);
    let changed = serve.next("config-changed");
    assert_eq!(changed["what"], json!(["data"]), "{changed}");
    let ready = serve.next("ready");
    assert_eq!(ready["features"]["data"]["enabled"], false, "{ready}");
    assert_eq!(graphql(&api_url), 404);
    app.edit_config(&[("  data:\n    enabled: false", "  data:\n    enabled: true")]);
    let from = serve.seen.len();
    let changed = serve.next("config-changed");
    assert_eq!(changed["what"], json!(["data"]), "{changed}");
    let schema = serve.seen_since(from, "schema");
    assert_eq!(
        schema.last().map(|event| &event["result"]),
        Some(&json!("unchanged"))
    );
    let ready = serve.next("ready");
    let entities = ready["features"]["data"]["entities"].as_array().unwrap();
    assert!(entities.contains(&json!("Todo")), "{ready}");
    assert_eq!(graphql(&api_url), 200);

    app.edit_config(&[
        (
            "  functions:\n    enabled: true",
            "  functions:\n    enabled: false",
        ),
        (
            "    password:\n      enabled: true",
            "    password:\n      enabled: false",
        ),
    ]);
    let from = serve.seen.len();
    let changed = serve.next("config-changed");
    assert_eq!(changed["what"], json!(["auth", "functions"]), "{changed}");
    let off = serve.seen_since(from, "functions");
    assert_eq!(off.last().map(|event| &event["state"]), Some(&json!("off")));
    let ready = serve.next("ready");
    assert_eq!(ready["apiUrl"], api_url.as_str());
    assert_eq!(ready["features"]["functions"]["enabled"], false, "{ready}");
    assert_eq!(ready["features"]["auth"]["password"], false, "{ready}");
    assert_eq!(invoke(&api_url, &key, "ping").0, 404);
    let (status, settings) = request(
        "GET",
        &format!("{api_url}/api/projectRuntimeSettings"),
        &[],
        None,
    );
    assert_eq!(status, 200, "{settings}");
    let settings: Value = serde_json::from_str(&settings).unwrap();
    assert_eq!(
        settings["serviceSettings"]["auth"]["password"],
        json!({ "enabled": false })
    );

    app.edit_config(&[(
        "  functions:\n    enabled: false",
        "  functions:\n    enabled: true",
    )]);
    let from = serve.seen.len();
    let changed = serve.next("config-changed");
    assert_eq!(changed["what"], json!(["functions"]), "{changed}");
    let built = serve.seen_since(from, "functions");
    assert_eq!(
        built.last().map(|event| &event["state"]),
        Some(&json!("ready"))
    );
    let ready = serve.next("ready");
    assert_eq!(ready["apiUrl"], api_url.as_str());
    let names = &ready["features"]["functions"]["names"];
    assert!(
        names.as_array().unwrap().contains(&json!("ping")),
        "{ready}"
    );
    assert_eq!(invoke(&api_url, &key, "ping"), (200, json!("pong")));

    // Settings that can't change while serving ask for a restart, and apply nothing.
    std::fs::write(
        app.path().join("rayfin-local.toml"),
        "[tokens]\nsource = \"az\"\n",
    )
    .unwrap();
    let changed = serve.next("config-changed");
    assert_eq!(changed["applied"], false, "{changed}");
    assert_eq!(changed["restartRequired"], true, "{changed}");
    assert_eq!(changed["what"], json!(["tokens"]), "{changed}");
    assert!(
        changed["message"].as_str().unwrap().contains("[tokens]"),
        "{changed}"
    );

    // A configuration that can't be read leaves the previous one in use.
    std::fs::write(app.config_path(), "services: [\n").unwrap();
    let error = serve.until("a `config-invalid` error", STEP, |event| {
        event["type"] == "error"
    });
    assert_eq!(error["code"], "config-invalid", "{error}");
    assert_eq!(error["fatal"], false, "{error}");
    assert_eq!(invoke(&api_url, &key, "ping"), (200, json!("pong")));

    assert_eq!(serve.close_stdin().code(), Some(0), "{}", serve.report());
}

#[test]
fn new_secret_values_restart_only_the_function_host() {
    let Some(app) = App::copy() else {
        return;
    };
    let mut serve = Serve::start(app.path(), &["--no-frontend", "--port", "0"]);
    let ready = serve.next("ready");
    let api_url = ready["apiUrl"].as_str().unwrap().to_owned();
    let key = publishable_key(&api_url);
    let fixture_value = json!({ "matchesFixture": true, "length": 18 });
    assert_eq!(invoke(&api_url, &key, "secretInfo"), (200, fixture_value));
    let hosts = started_by(serve.child.id(), "function-host");
    assert_eq!(hosts.len(), 1, "{hosts:?}");

    let mut set = rayfin_local()
        .args(["secret", "set", "GREETING_SECRET", "-C"])
        .arg(app.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    set.stdin.take().unwrap().write_all(b"rotated\n").unwrap();
    let output = set.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("rotated"),
        "the value was printed"
    );

    let from = serve.seen.len();
    let changed = serve.next("config-changed");
    assert_eq!(changed["applied"], true, "{changed}");
    assert_eq!(changed["what"], json!(["secrets"]), "{changed}");
    let restarted = serve.seen_since(from, "functions");
    assert_eq!(
        restarted.last().map(|event| &event["state"]),
        Some(&json!("ready")),
        "{}",
        serve.report()
    );
    let ready = serve.next("ready");
    assert_eq!(ready["apiUrl"], api_url.as_str());
    assert!(
        !serve
            .seen
            .iter()
            .any(|event| event.to_string().contains("rotated"))
    );
    assert_eq!(
        invoke(&api_url, &key, "secretInfo"),
        (200, json!({ "matchesFixture": false, "length": 7 }))
    );
    replaced(serve.child.id(), "function-host", hosts[0]);

    assert_eq!(serve.close_stdin().code(), Some(0), "{}", serve.report());
}

#[test]
fn vite_starts_again_on_its_port_after_it_is_killed() {
    let Some(app) = App::copy() else {
        return;
    };
    let mut serve = Serve::start(
        app.path(),
        &["--no-functions", "--port", "0", "--frontend-port", "0"],
    );
    let ready = serve.next("ready");
    let app_url = ready["appUrl"].as_str().unwrap().to_owned();
    assert_eq!(request("GET", &app_url, &[], None).0, 200);
    let vite = started_by(serve.child.id(), "vite-dev");
    assert_eq!(vite.len(), 1, "{vite:?}");

    kill(vite[0]);
    let vite_state = |state: &'static str| {
        move |event: &Value| event["type"] == "vite" && event["state"] == state
    };
    let exited = serve.until("Vite's exit", STEP, vite_state("exited"));
    assert_eq!(exited["url"], app_url.as_str());
    serve.until("Vite's restart", STEP, vite_state("restarting"));
    let back = serve.until("Vite back", STEP, vite_state("ready"));
    assert_eq!(back["url"], app_url.as_str(), "{back}");
    assert_eq!(request("GET", &app_url, &[], None).0, 200);
    replaced(serve.child.id(), "vite-dev", vite[0]);

    // A change to rayfin.yml keeps the app's URL too.
    app.edit_config(&[(
        "  storage:\n    enabled: true",
        "  storage:\n    enabled: false",
    )]);
    serve.next("config-changed");
    let changed = serve.next("ready");
    assert_eq!(changed["appUrl"], app_url.as_str(), "{changed}");
    assert_eq!(changed["apiUrl"], ready["apiUrl"], "{changed}");

    assert_eq!(serve.close_stdin().code(), Some(0), "{}", serve.report());
}
