//! The host contract (docs/embedding.md) of the built binary: `version --json`, fatal errors of
//! `serve --json`, and a `serve --json` of the fixture app from its start to the end of stdin.
//!
//! The last needs Node.js and the fixture's npm packages (`npm ci` in
//! `tests/fixtures/apps/basic`); without them it prints a note and passes.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::Value;

fn rayfin_local() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rayfin-local"))
}

#[test]
fn version_json_reports_the_contract_and_the_sdk() {
    let output = rayfin_local().args(["version", "--json"]).output().unwrap();
    assert!(output.status.success());
    let version: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(version["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(version["contract"], 1);
    let supported = version["sdk"]["supported"].as_str().unwrap();
    assert!(
        supported.starts_with(">=") && supported.contains(" <"),
        "{supported}"
    );
    assert!(version["sdk"]["tested"].is_string());

    let output = rayfin_local().arg("version").output().unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("rayfin-local {}\n", env!("CARGO_PKG_VERSION"))
    );
}

/// A `serve --json` that runs until stdin ends, its events read as they come.
struct Serve {
    child: Child,
    events: Receiver<Value>,
    stderr: Receiver<String>,
    seen: Vec<Value>,
}

impl Serve {
    fn start(project: &Path, args: &[&str], env: &[(&str, &str)]) -> Self {
        let mut command = rayfin_local();
        command
            .args(["serve", "--json", "-C"])
            .arg(project)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command.spawn().unwrap();
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

    /// The events until one of type `wanted` (included), which is returned.
    fn until(&mut self, wanted: &str, limit: Duration) -> Value {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Ok(event) = self.events.recv_timeout(left) else {
                panic!("no `{wanted}` event in {limit:?}:\n{}", self.report());
            };
            check_envelope(&event);
            self.seen.push(event.clone());
            if event["type"] == wanted {
                return event;
            }
            if event["type"] == "stopped" {
                panic!("stopped before `{wanted}`:\n{}", self.report());
            }
        }
    }

    /// Closes stdin, then waits for the exit and the last events.
    fn close_stdin(&mut self, limit: Duration) -> ExitStatus {
        drop(self.child.stdin.take());
        self.wait(limit)
    }

    fn wait(&mut self, limit: Duration) -> ExitStatus {
        let deadline = Instant::now() + limit;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("rayfin-local did not exit in {limit:?}:\n{}", self.report());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        while let Ok(event) = self.events.recv_timeout(Duration::from_secs(5)) {
            check_envelope(&event);
            self.seen.push(event);
        }
        status
    }

    /// The types of the events seen, without `log`, `warning` and `error`.
    fn milestones(&self) -> Vec<&str> {
        self.seen
            .iter()
            .filter_map(|event| event["type"].as_str())
            .filter(|kind| !matches!(*kind, "log" | "warning" | "error"))
            .collect()
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

fn check_envelope(event: &Value) {
    assert_eq!(event["v"], 1, "{event}");
    let time = event["time"].as_str().unwrap_or_default();
    let shape =
        time.len() == 24 && &time[10..11] == "T" && &time[19..20] == "." && time.ends_with('Z');
    assert!(shape, "time {time:?} in {event}");
}

#[test]
fn fatal_errors_come_with_a_code_then_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let mut serve = Serve::start(dir.path(), &[], &[]);
    let error = serve.until("error", Duration::from_secs(30));
    assert_eq!(error["code"], "project-not-found");
    assert_eq!(error["fatal"], true);
    assert_eq!(serve.wait(Duration::from_secs(30)).code(), Some(1));
    assert_eq!(serve.milestones(), ["stopped"]);
    let stopped = serve.seen.last().unwrap();
    assert_eq!(stopped["reason"], "error");
    assert_eq!(stopped["exitCode"], 1);

    std::fs::create_dir_all(dir.path().join("rayfin")).unwrap();
    std::fs::write(dir.path().join("rayfin").join("rayfin.yml"), "name: app\n").unwrap();
    let short_token = [("RAYFIN_LOCAL_CONTROL_TOKEN", "too-short")];
    let mut serve = Serve::start(dir.path(), &[], &short_token);
    let error = serve.until("error", Duration::from_secs(30));
    assert_eq!(error["code"], "config-invalid");
    assert!(!error.to_string().contains("too-short"), "{error}");
    assert_eq!(serve.wait(Duration::from_secs(30)).code(), Some(1));
    assert_eq!(serve.milestones(), ["starting", "stopped"]);
    assert_eq!(serve.seen[0]["project"]["name"], "app");
}

/// `tests/fixtures/apps/basic`, when its packages and Node.js are installed.
fn fixture() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/apps/basic");
    let installed = ["@microsoft/rayfin-core", "vite"].iter().all(|package| {
        let manifest = dir.join("node_modules").join(package).join("package.json");
        manifest.is_file()
    });
    if !installed {
        eprintln!(
            "skipped: the fixture's packages are not installed (run `npm ci` in {})",
            dir.display()
        );
        return None;
    }
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("skipped: Node.js is not on PATH");
        return None;
    }
    Some(dir)
}

#[test]
fn serve_json_starts_the_app_and_stops_when_stdin_closes() {
    let Some(project) = fixture() else {
        return;
    };
    let mut serve = Serve::start(
        &project,
        &["--no-functions", "--port", "0", "--frontend-port", "0"],
        &[],
    );
    let ready = serve.until("ready", Duration::from_secs(300));
    assert_eq!(
        serve.milestones(),
        ["starting", "analysis", "schema", "vite", "ready"],
        "{}",
        serve.report()
    );
    assert_eq!(ready["features"]["functions"]["enabled"], false);
    let api_url = ready["apiUrl"].as_str().unwrap();
    let (status, body) = get(&format!("{api_url}/.rayfin-local/health"));
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#"{"ok":true}"#), "{body}");
    let (status, _) = get(&format!("{api_url}/.rayfin-local/control/status"));
    assert_eq!(
        status, 404,
        "no control API without RAYFIN_LOCAL_CONTROL_TOKEN"
    );
    let app_url = ready["appUrl"].as_str().unwrap();
    assert_eq!(get(app_url).0, 200);

    let started = descendants(serve.child.id());
    assert!(!started.is_empty(), "Vite is not a child of rayfin-local");
    let status = serve.close_stdin(Duration::from_secs(60));
    assert_eq!(status.code(), Some(0), "{}", serve.report());
    let stopped = serve.seen.last().unwrap();
    assert_eq!(stopped["type"], "stopped", "{}", serve.report());
    assert_eq!(stopped["reason"], "stdin-closed");
    assert_eq!(stopped["exitCode"], 0);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let running: BTreeSet<u32> = processes().into_iter().map(|(pid, _)| pid).collect();
        let left: Vec<&u32> = started.iter().filter(|pid| running.contains(pid)).collect();
        if left.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "still running: {left:?}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `GET url` over plain HTTP: the status and the whole response.
fn get(url: &str) -> (u16, String) {
    let rest = url.strip_prefix("http://").unwrap();
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = TcpStream::connect(authority).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    write!(
        stream,
        "GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .unwrap_or(0);
    (status, response)
}

/// Every process: its id and its parent's.
fn processes() -> Vec<(u32, u32)> {
    let output = if cfg!(windows) {
        let list = "Get-CimInstance Win32_Process | ForEach-Object { \
                    \"$($_.ProcessId) $($_.ParentProcessId)\" }";
        Command::new("powershell")
            .args(["-NoProfile", "-Command", list])
            .output()
    } else {
        Command::new("ps").args(["-A", "-o", "pid=,ppid="]).output()
    }
    .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut ids = line.split_whitespace().map(|id| id.parse().ok());
            Some((ids.next()??, ids.next()??))
        })
        .collect()
}

/// The processes `pid` started, and those they started.
fn descendants(pid: u32) -> Vec<u32> {
    let all = processes();
    let mut found = Vec::new();
    let mut parents = vec![pid];
    while let Some(parent) = parents.pop() {
        for &(child, of) in &all {
            if of == parent && child != parent && !found.contains(&child) {
                found.push(child);
                parents.push(child);
            }
        }
    }
    found
}
