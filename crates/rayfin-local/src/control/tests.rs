//! The control API's routes over the basic model: who may call them, and what each answers.

use std::net::SocketAddr;
use std::sync::Mutex;

use axum::body::Body;
use axum::http::{HeaderMap, Method};
use http_body_util::BodyExt;
use rl_auth::AuthSettings;
use rl_graphql::DataApiSettings;
use rl_model::{AppModel, CallerResolver};
use tower::ServiceExt;

use super::*;

const TOKEN: &str = "a-control-token-of-more-than-32-characters";
const C1: &str = "00000000-0000-4000-8000-0000000000c1";
const C2: &str = "00000000-0000-4000-8000-0000000000c2";
const T1: &str = "00000000-0000-4000-8000-000000000001";

/// `Authorization` for `token`.
fn bearer(token: &str) -> String {
    ["Bearer", token].join(" ")
}

struct Harness {
    _dir: tempfile::TempDir,
    router: Router,
    auth: Arc<AuthService>,
    db: Db,
    storage_dir: PathBuf,
    live: LiveState,
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

/// Who sends a request: a loopback host with the token, unless changed.
struct Sender<'a> {
    peer: Option<[u8; 4]>,
    token: Option<&'a str>,
    origin: Option<&'a str>,
}

const HOST: Sender<'static> = Sender {
    peer: Some([127, 0, 0, 1]),
    token: Some(TOKEN),
    origin: None,
};

async fn harness() -> Harness {
    harness_with(Arc::new(FakeReload::default())).await
}

async fn harness_with(reload: Arc<dyn ReloadTrigger>) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("db.sqlite")).unwrap();
    let analysis = include_str!("../../../rl-model/tests/fixtures/basic.analysis.json");
    let model =
        Arc::new(AppModel::from_analysis(&serde_json::from_str(analysis).unwrap()).unwrap());
    rl_db::apply(&db, &model, false).await.unwrap();
    let auth = Arc::new(
        AuthService::new(
            db.clone(),
            dir.path().join("keys"),
            AuthSettings::new("6c6f6361-6c00-4000-8000-0000000000c0"),
        )
        .await
        .unwrap(),
    );
    let callers: Arc<dyn CallerResolver> = auth.clone();
    let data = DataApi::new(db.clone(), model, callers, DataApiSettings::default());
    let storage_dir = dir.path().join("storage");
    let live = LiveState::default();
    let router = Control {
        token: TOKEN.to_owned(),
        auth: Arc::clone(&auth),
        db: db.clone(),
        data: Some(Arc::new(data)),
        functions: FunctionsSlot::default(),
        storage_dir: storage_dir.clone(),
        reload,
        live: live.clone(),
    }
    .router();
    Harness {
        _dir: dir,
        router,
        auth,
        db,
        storage_dir,
        live,
    }
}

impl Harness {
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Answer {
        self.call_as(&HOST, method, path, body).await
    }

    async fn call_as(
        &self,
        sender: &Sender<'_>,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Answer {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(format!("/.rayfin-local/control{path}"));
        if let Some(ip) = sender.peer {
            request = request.extension(ConnectInfo(SocketAddr::from((ip, 50123))));
        }
        if let Some(token) = sender.token {
            request = request.header(header::AUTHORIZATION, bearer(token));
        }
        if let Some(origin) = sender.origin {
            request = request.header(header::ORIGIN, origin);
        }
        let body = match body {
            Some(json) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let raw = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        Answer {
            status,
            headers,
            body,
        }
    }

    /// Two categories, the second the first's child, and a todo in the first.
    fn seed_rows(&self) {
        let sql = format!(
            "INSERT INTO \"Categories\" (id, name, display_label, sortOrder, parent_id) VALUES
               ('{C1}', 'Home', 'House', 1, NULL), ('{C2}', 'Work', NULL, 2, '{C1}');
             INSERT INTO \"Todos\" (id, user_id, title, status, cost, isPublic, createdAt,
               category_id)
             VALUES ('{T1}', 'u1', 'Buy milk', 'doing', 1.5, 1, '2026-01-01T00:00:00.0000000Z',
               '{C1}');"
        );
        self.db
            .call_blocking(|conn| conn.execute_batch(&sql))
            .unwrap();
    }
}

/// The error code of an answer.
fn code(answer: &Answer) -> &str {
    answer.body["error"]["code"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn only_the_host_on_this_machine_is_answered() {
    let h = harness().await;
    let refused = [
        (
            "a remote peer",
            Sender {
                peer: Some([192, 168, 1, 20]),
                ..HOST
            },
            StatusCode::FORBIDDEN,
        ),
        (
            "no peer",
            Sender { peer: None, ..HOST },
            StatusCode::FORBIDDEN,
        ),
        (
            "a web page",
            Sender {
                origin: Some("http://localhost:5173"),
                ..HOST
            },
            StatusCode::FORBIDDEN,
        ),
        (
            "no token",
            Sender {
                token: None,
                ..HOST
            },
            StatusCode::UNAUTHORIZED,
        ),
        (
            "a wrong token",
            Sender {
                token: Some("a-control-token-of-more-than-32-characterz"),
                ..HOST
            },
            StatusCode::UNAUTHORIZED,
        ),
    ];
    for (case, sender, status) in refused {
        let answer = h.call_as(&sender, Method::GET, "/users", None).await;
        assert_eq!(answer.status, status, "{case}: {}", answer.body);
        assert!(
            ["forbidden", "unauthorized"].contains(&code(&answer)),
            "{case}"
        );
        assert!(answer.body["error"]["message"].is_string(), "{case}");
        if status == StatusCode::UNAUTHORIZED {
            assert_eq!(answer.headers[header::WWW_AUTHENTICATE], "Bearer", "{case}");
        }
    }

    let answer = h.call(Method::GET, "/users", None).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(answer.body, json!({ "users": [] }));
}

#[test]
fn secrets_compare_whole() {
    assert!(same_secret(TOKEN, TOKEN));
    assert!(!same_secret(&TOKEN[1..], TOKEN));
    assert!(!same_secret("", TOKEN));
    assert!(is_loopback("::ffff:127.0.0.1".parse().unwrap()));
    assert!(!is_loopback("10.0.0.1".parse().unwrap()));
}

#[tokio::test]
async fn the_status_is_the_ready_payload_with_the_identity_and_entities_as_they_are_now() {
    let h = harness().await;
    let starting = h.call(Method::GET, "/status", None).await;
    assert_eq!(starting.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code(&starting), "starting");

    h.live.record(&json!({
        "v": 1, "type": "ready", "time": "2026-10-30T03:12:09.512Z",
        "apiUrl": "http://localhost:7171", "signInAs": { "kind": "developer" },
        "features": { "data": { "enabled": true, "entities": ["Old"] } },
    }));
    h.live
        .record(&json!({ "v": 1, "type": "schema", "result": "applied" }));
    h.live
        .record(&json!({ "v": 1, "type": "analysis", "ok": true }));
    let status = h.call(Method::GET, "/status", None).await;
    assert_eq!(status.status, StatusCode::OK);
    assert_eq!(status.body["state"], "running");
    assert_eq!(status.body["apiUrl"], "http://localhost:7171");
    assert_eq!(status.body["schema"]["result"], "applied");
    assert_eq!(status.body["analysis"]["ok"], true);
    assert_eq!(status.body["functions"], Value::Null);
    assert!(status.body.get("type").is_none());
    // The entities the data API serves, which changed since `ready`.
    let entities = status.body["features"]["data"]["entities"].clone();
    assert!(entities.as_array().unwrap().contains(&json!("Category")));
    assert!(!entities.as_array().unwrap().contains(&json!("Old")));

    h.auth
        .switch_sign_in_as(SignInAs::User("ana@example.com".into()))
        .await
        .unwrap();
    let status = h.call(Method::GET, "/status", None).await;
    assert_eq!(
        status.body["signInAs"],
        json!({ "kind": "user", "username": "ana@example.com" })
    );
}

#[tokio::test]
async fn changing_the_identity_signs_everyone_out() {
    let h = harness().await;
    let identity = h.call(Method::GET, "/identity", None).await;
    assert_eq!(identity.body, json!({ "kind": "developer", "user": null }));
    let bob = h.auth.create_user("bob", "secret1").await.unwrap();
    let session = h.auth.issue_session(&bob).await.unwrap();

    let missing = json!({ "kind": "user", "user": "ana@example.com" });
    let answer = h.call(Method::PUT, "/identity", Some(missing)).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(code(&answer), "user-not-found");
    assert!(h.auth.resolve(Some(&session.access_token)).is_ok());

    let created = json!({ "kind": "user", "user": " ana@example.com ", "create": true });
    let answer = h.call(Method::PUT, "/identity", Some(created)).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    assert_eq!(answer.body["kind"], "user");
    let ana = &answer.body["user"];
    assert_eq!(ana["username"], "ana@example.com");
    assert_eq!(ana["email"], "ana@example.com");
    assert_eq!(ana["signIn"], json!([]));
    assert_eq!(ana["developer"], false);
    assert!(h.auth.resolve(Some(&session.access_token)).is_err());
    assert_eq!(
        h.call(Method::GET, "/identity", None).await.body,
        answer.body
    );

    let nobody = h
        .call(Method::PUT, "/identity", Some(json!({ "kind": "none" })))
        .await;
    assert_eq!(nobody.body, json!({ "kind": "none", "user": null }));
    for wrong in [
        json!({ "kind": "admin" }),
        json!({ "kind": "user" }),
        json!({ "kind": "user", "user": "  " }),
        json!("developer"),
    ] {
        let answer = h.call(Method::PUT, "/identity", Some(wrong.clone())).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{wrong}");
        assert_eq!(code(&answer), "invalid-request", "{wrong}");
    }

    let developer = json!({ "kind": "developer" });
    let answer = h.call(Method::PUT, "/identity", Some(developer)).await;
    assert_eq!(answer.body, json!({ "kind": "developer", "user": null }));
    h.call(Method::POST, "/token", None).await;
    let identity = h.call(Method::GET, "/identity", None).await;
    assert_eq!(identity.body["user"]["username"], rl_auth::LOCAL_DEVELOPER);
    assert_eq!(identity.body["user"]["developer"], true);
}

#[tokio::test]
async fn users_are_listed_added_and_deleted() {
    let h = harness().await;
    let ana = json!({ "username": "ana@example.com", "password": "secret1" });
    let added = h.call(Method::POST, "/users", Some(ana)).await;
    assert_eq!(added.status, StatusCode::CREATED, "{}", added.body);
    let keys: Vec<&str> = added
        .body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "id",
            "username",
            "email",
            "createdAt",
            "signIn",
            "developer"
        ]
    );
    assert_eq!(added.body["signIn"], json!(["password"]));
    assert_eq!(added.body["developer"], false);
    let created_at = added.body["createdAt"].as_str().unwrap();
    assert!(
        created_at.len() == 24 && created_at.ends_with('Z'),
        "{created_at}"
    );

    let taken = json!({ "username": "Ana@Example.com" });
    let answer = h.call(Method::POST, "/users", Some(taken)).await;
    assert_eq!(answer.status, StatusCode::CONFLICT);
    assert_eq!(code(&answer), "user-exists");
    let bob = h
        .call(Method::POST, "/users", Some(json!({ "username": "bob" })))
        .await;
    assert_eq!(bob.status, StatusCode::CREATED);
    assert_eq!(bob.body["email"], Value::Null);
    assert_eq!(bob.body["signIn"], json!([]));
    for wrong in [json!({}), json!({ "username": "carol", "password": " " })] {
        let answer = h.call(Method::POST, "/users", Some(wrong.clone())).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{wrong}");
    }

    let users = h.call(Method::GET, "/users", None).await;
    let names: Vec<&str> = users.body["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|user| user["username"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["ana@example.com", "bob"]);

    let deleted = h.call(Method::DELETE, "/users/bob", None).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let again = h.call(Method::DELETE, "/users/bob", None).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
    assert_eq!(code(&again), "user-not-found");
    let id = added.body["id"].as_str().unwrap();
    let by_id = h.call(Method::DELETE, &format!("/users/{id}"), None).await;
    assert_eq!(by_id.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn tokens_act_as_the_developer_or_a_user() {
    let h = harness().await;
    let user_of = |answer: &Answer| {
        let token = answer.body["accessToken"].as_str().unwrap();
        h.auth
            .resolve(Some(token))
            .unwrap()
            .session
            .unwrap()
            .user_id
    };
    let developer = h
        .call(Method::POST, "/token", Some(json!({ "user": null })))
        .await;
    assert_eq!(developer.status, StatusCode::OK, "{}", developer.body);
    assert_eq!(developer.body["tokenType"], "Bearer");
    assert_eq!(developer.body["expiresIn"], 3600);
    assert_eq!(developer.body["user"]["username"], rl_auth::LOCAL_DEVELOPER);
    assert_eq!(developer.body["user"]["developer"], true);
    assert_eq!(user_of(&developer), developer.body["user"]["id"]);
    assert!(developer.body.get("refreshToken").is_none());

    let missing = json!({ "user": "carol" });
    let answer = h.call(Method::POST, "/token", Some(missing)).await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(code(&answer), "user-not-found");
    let created = json!({ "user": "carol", "create": true });
    let carol = h.call(Method::POST, "/token", Some(created)).await;
    assert_eq!(carol.body["user"]["username"], "carol");
    assert_eq!(user_of(&carol), carol.body["user"]["id"]);
}

#[tokio::test]
async fn entities_and_their_rows() {
    let h = harness().await;
    h.seed_rows();
    let entities = h.call(Method::GET, "/entities", None).await;
    let list = entities.body["entities"].as_array().unwrap();
    assert!(list.contains(&json!({ "name": "Category", "rows": 2 })));
    assert!(list.contains(&json!({ "name": "Todo", "rows": 1 })));
    assert!(list.contains(&json!({ "name": "Comment", "rows": 0 })));

    let page = h
        .call(
            Method::GET,
            "/entities/Category/rows?orderBy=name&desc=true&limit=1&offset=0",
            None,
        )
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert_eq!(page.body["total"], 2);
    assert_eq!(page.body["rows"].as_array().unwrap().len(), 1);
    assert_eq!(page.body["rows"][0]["name"], "Work");
    assert_eq!(
        page.body["columns"][0],
        json!({ "name": "id", "type": "UUID", "key": true })
    );
    let todos = h.call(Method::GET, "/entities/Todo/rows", None).await;
    assert_eq!(todos.body["rows"][0]["cost"], json!(1.5));
    assert_eq!(todos.body["rows"][0]["isPublic"], json!(true));

    for (query, wanted) in [
        ("limit=501", "invalid-request"),
        ("limit=0", "invalid-request"),
        ("offset=-1", "invalid-request"),
        ("desc=maybe", "invalid-request"),
        ("orderBy=nope", "invalid-order-by"),
    ] {
        let path = format!("/entities/Category/rows?{query}");
        let answer = h.call(Method::GET, &path, None).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(code(&answer), wanted, "{query}");
    }
    let unknown = h.call(Method::GET, "/entities/Nope/rows", None).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(code(&unknown), "entity-not-found");
}

#[tokio::test]
async fn rows_are_cleared_unless_others_refer_to_them_and_the_data_is_reset() {
    let h = harness().await;
    h.seed_rows();
    let refused = h
        .call(Method::DELETE, "/entities/Category/rows", None)
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(code(&refused), "rows-referenced");
    assert!(
        refused.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("1 Todo row"),
        "{}",
        refused.body
    );
    let todos = h.call(Method::DELETE, "/entities/Todo/rows", None).await;
    assert_eq!(todos.body, json!({ "deleted": 1 }));
    let categories = h
        .call(Method::DELETE, "/entities/Category/rows", None)
        .await;
    assert_eq!(categories.body, json!({ "deleted": 2 }));

    h.seed_rows();
    let ana = h.auth.create_user("ana", "secret1").await.unwrap();
    let session = h.auth.issue_session(&ana).await.unwrap();
    let objects = h.storage_dir.join("objects");
    std::fs::create_dir_all(&objects).unwrap();
    std::fs::write(objects.join("blob"), "stored").unwrap();
    let reset = h.call(Method::POST, "/data/reset", Some(json!({}))).await;
    assert_eq!(reset.status, StatusCode::OK, "{}", reset.body);
    assert_eq!(reset.body, json!({ "ok": true }));
    let entities = h.call(Method::GET, "/entities", None).await;
    assert!(
        entities.body["entities"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entity| entity["rows"] == 0),
        "{}",
        entities.body
    );
    assert!(h.auth.list_users().await.unwrap().is_empty());
    assert!(h.auth.resolve(Some(&session.access_token)).is_err());
    assert_eq!(std::fs::read_dir(&objects).unwrap().count(), 0);
}

/// Records each reload and answers what it is told to, or as live reload would.
#[derive(Default)]
struct FakeReload {
    forced: Mutex<Vec<bool>>,
    answers: Mutex<Vec<anyhow::Result<ReloadOutcome>>>,
}

impl ReloadTrigger for FakeReload {
    fn reload(&self, force_schema: bool) -> BoxFuture<'_, anyhow::Result<ReloadOutcome>> {
        self.forced.lock().unwrap().push(force_schema);
        let result = if force_schema { "applied" } else { "refused" };
        let answer = self.answers.lock().unwrap().pop().unwrap_or_else(|| {
            Ok(ReloadOutcome {
                schema: json!({ "v": 1, "type": "schema", "result": result, "live": true }),
                config: None,
            })
        });
        Box::pin(async move { answer })
    }
}

#[tokio::test]
async fn reloads_answer_their_events_or_why_they_failed() {
    let reload = Arc::new(FakeReload::default());
    let h = harness_with(Arc::clone(&reload) as Arc<dyn ReloadTrigger>).await;
    let refused = h.call(Method::POST, "/reload", None).await;
    assert_eq!(refused.status, StatusCode::OK, "{}", refused.body);
    assert_eq!(refused.body["schema"]["result"], "refused");
    assert_eq!(refused.body["config"], Value::Null);
    let forced = json!({ "forceSchema": true });
    let applied = h.call(Method::POST, "/reload", Some(forced)).await;
    assert_eq!(applied.body["schema"]["result"], "applied");
    assert_eq!(*reload.forced.lock().unwrap(), [false, true]);

    let failures = [
        (
            crate::errors::coded(ErrorCode::ConfigInvalid, json!({}), anyhow::anyhow!("bad")),
            StatusCode::UNPROCESSABLE_ENTITY,
            "config-invalid",
        ),
        (
            crate::errors::coded(ErrorCode::AnalysisFailed, json!({}), anyhow::anyhow!("TS")),
            StatusCode::UNPROCESSABLE_ENTITY,
            "analysis-failed",
        ),
        (
            anyhow::Error::new(Stopping),
            StatusCode::SERVICE_UNAVAILABLE,
            "stopping",
        ),
        (
            anyhow::anyhow!("the reloader broke"),
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
        ),
    ];
    for (failure, status, expected) in failures {
        let message = failure.to_string();
        reload.answers.lock().unwrap().push(Err(failure));
        let answer = h.call(Method::POST, "/reload", None).await;
        assert_eq!((answer.status, code(&answer)), (status, expected));
        let said = answer.body["error"]["message"].as_str().unwrap();
        assert!(said.contains(&message), "{said}");
        assert!(answer.body["error"]["hint"].is_string(), "{}", answer.body);
    }
}
