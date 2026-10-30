//! Automatic sign-in and the control API (docs/embedding.md) of the built binary: a host runs
//! `serve --json` on the fixture app with a control token, then drives it the way Fabricator
//! does, and signs the app in through the app's dev server as its page would.
//!
//! It needs Node.js and the fixture's npm packages (`npm ci` in `tests/fixtures/apps/basic`);
//! without them it prints a note and passes.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const TOKEN: &str = "0123456789abcdef0123456789abcdef-control-api-test";

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

/// A `serve --json` that runs until stdin closes.
struct Serve {
    child: Child,
    events: Receiver<Value>,
}

impl Serve {
    fn start(project: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rayfin-local"))
            .args(["serve", "--json", "--reset", "--no-functions"])
            .args(["--port", "0", "--frontend-port", "0", "-C"])
            .arg(project)
            .env("RAYFIN_LOCAL_CONTROL_TOKEN", TOKEN)
            // Not a user's token: the developer identity signs in as the local developer account,
            // whatever Azure sign-in this machine has.
            .env("RAYFIN_LOCAL_TOKEN_POWERBI", "not-a-user-token")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let (sender, events) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(event) = serde_json::from_str::<Value>(&line)
                    && sender.send(event).is_err()
                {
                    break;
                }
            }
        });
        Self { child, events }
    }

    /// The next event of type `wanted`.
    fn until(&self, wanted: &str) -> Value {
        loop {
            let event = self
                .events
                .recv_timeout(Duration::from_secs(300))
                .unwrap_or_else(|_| panic!("no `{wanted}` event"));
            assert_ne!(
                event["type"], "stopped",
                "stopped before `{wanted}`: {event}"
            );
            if event["type"] == wanted {
                return event;
            }
        }
    }

    fn stop(mut self) -> Option<i32> {
        drop(self.child.stdin.take());
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The control API of a running `serve`.
struct Host {
    client: reqwest::Client,
    api_url: String,
}

impl Host {
    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/.rayfin-local/control{path}", self.api_url);
        self.client.request(method, url).bearer_auth(TOKEN)
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut request = self.request(method, path);
        if let Some(body) = body {
            request = request.json(&body);
        }
        answer(request).await
    }
}

async fn answer(request: reqwest::RequestBuilder) -> (StatusCode, Value) {
    let response = request.send().await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_drives_the_app_and_signs_it_in_as_anyone() {
    let Some(project) = fixture() else {
        return;
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let serve = Serve::start(&project);
    let ready = serve.until("ready");
    assert_eq!(ready["signInAs"], json!({ "kind": "developer" }));
    let api_url = ready["apiUrl"].as_str().unwrap().to_owned();
    let app_url = ready["appUrl"]
        .as_str()
        .unwrap()
        .trim_end_matches('/')
        .to_owned();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let host = Host {
        client: client.clone(),
        api_url: api_url.clone(),
    };

    let (status, body) = host.call(Method::GET, "/status", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "running");
    assert_eq!(body["apiUrl"], api_url);
    assert_eq!(body["schema"]["type"], "schema");
    let control = format!("{api_url}/.rayfin-local/control/status");
    let (status, _) = answer(client.get(&control)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let from_a_page = client
        .get(&control)
        .bearer_auth(TOKEN)
        .header("origin", app_url.as_str());
    let (status, _) = answer(from_a_page).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) =
        answer(client.get(format!("{api_url}/.rayfin-local/dev/session-token"))).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "only the dev server signs in"
    );

    // The page asks its dev server, as fetchRayfinLocalSessionToken() does: a same-origin GET.
    let sign_in = || {
        client
            .get(format!("{app_url}/.rayfin/dev/session-token"))
            .header("accept", "application/json")
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-mode", "cors")
            .header("referer", format!("{app_url}/"))
    };
    let (status, developer) = answer(sign_in()).await;
    assert_eq!(status, StatusCode::OK, "{developer}");
    assert_eq!(developer["tokenType"], "Bearer");
    assert!(developer["refreshToken"].is_string());
    let (_, identity) = host.call(Method::GET, "/identity", None).await;
    assert_eq!(identity["user"]["username"], "developer@example.com");
    assert_eq!(identity["user"]["developer"], true);

    let ana = json!({ "username": "ana@example.com", "password": "secret-1" });
    let (status, ana) = host.call(Method::POST, "/users", Some(ana)).await;
    assert_eq!(status, StatusCode::CREATED, "{ana}");
    let as_ana = json!({ "kind": "user", "user": "ana@example.com" });
    let (status, identity) = host.call(Method::PUT, "/identity", Some(as_ana)).await;
    assert_eq!(status, StatusCode::OK, "{identity}");
    assert_eq!(identity["user"]["id"], ana["id"]);
    let (status, signed_in) = answer(sign_in()).await;
    assert_eq!(status, StatusCode::OK, "{signed_in}");
    let access_token = signed_in["accessToken"].as_str().unwrap().to_owned();
    let (_, status_now) = host.call(Method::GET, "/status", None).await;
    assert_eq!(
        status_now["signInAs"],
        json!({ "kind": "user", "username": "ana@example.com" })
    );

    // The app's own routes, as the signed-in user.
    let config: Value = client
        .get(format!("{api_url}/rayfin.config.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let key = config["publishableKey"].as_str().unwrap().to_owned();
    let graphql = |query: &str, token: &str| {
        client
            .post(format!("{api_url}/graphql"))
            .bearer_auth(token)
            .header("x-publishable-key", key.as_str())
            .json(&json!({ "query": query }))
    };
    let create = "mutation { createCategory(item: { name: \"From a host\", sortOrder: 3 }) \
                  { id name } }";
    let (status, created) = answer(graphql(create, &access_token)).await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["data"]["createCategory"]["name"], "From a host");

    let (_, entities) = host.call(Method::GET, "/entities", None).await;
    let entities = entities["entities"].as_array().unwrap().clone();
    assert!(entities.contains(&json!({ "name": "Category", "rows": 1 })));
    let (status, rows) = host
        .call(
            Method::GET,
            "/entities/Category/rows?limit=10&orderBy=name",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    assert_eq!(rows["total"], 1);
    assert_eq!(rows["rows"][0]["name"], "From a host");
    assert_eq!(rows["rows"][0]["sortOrder"], 3);

    let (status, token) = host
        .call(
            Method::POST,
            "/token",
            Some(json!({ "user": "bob", "create": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{token}");
    let bob_token = token["accessToken"].as_str().unwrap();
    let (status, listed) = answer(graphql("{ categories { items { name } } }", bob_token)).await;
    assert_eq!(status, StatusCode::OK, "{listed}");

    let nobody = json!({ "kind": "none" });
    host.call(Method::PUT, "/identity", Some(nobody)).await;
    let (status, refused) = answer(sign_in()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused["error"], "Automatic sign-in is off.");
    let (status, _) = answer(graphql("{ categories { items { name } } }", &access_token)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "switching signed ana out");

    let (status, reload) = host.call(Method::POST, "/reload", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK, "{reload}");
    assert_eq!(reload["schema"]["result"], "unchanged", "{reload}");
    assert_eq!(reload["config"], Value::Null, "{reload}");

    let (status, reset) = host.call(Method::POST, "/data/reset", None).await;
    assert_eq!((status, reset), (StatusCode::OK, json!({ "ok": true })));
    let (_, entities) = host.call(Method::GET, "/entities", None).await;
    assert!(
        entities["entities"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entity| entity["rows"] == 0)
    );
    let (_, users) = host.call(Method::GET, "/users", None).await;
    assert_eq!(users, json!({ "users": [] }));

    assert_eq!(serve.stop(), Some(0));
}
