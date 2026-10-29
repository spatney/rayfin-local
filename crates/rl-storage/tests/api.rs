//! The storage routes driven as the 1.36.2 client drives them (`StorageFolderClient.js`), on the
//! `basic` fixture's folders plus a public one.

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode},
};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use rl_db::Db;
use rl_model::{
    Action, AppModel, Caller, CallerError, CallerResolver, OnConflict, Session, StorageFolder,
    StorageRule,
};
use rl_storage::StorageConfig;
use serde_json::{Map, Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const ORIGIN: &str = "http://localhost:7171";
const HELLO_SHA256: &str = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

/// `Bearer <user>` signs in as `<user>`; `Bearer bad` is an invalid token.
struct Tokens;

impl CallerResolver for Tokens {
    fn resolve(&self, bearer: Option<&str>) -> Result<Caller, CallerError> {
        match bearer {
            None => Ok(Caller::anonymous()),
            Some("bad") => Err(CallerError::InvalidToken),
            Some(user) => Ok(Caller::authenticated(Session {
                user_id: user.into(),
                email: None,
                claims: Map::new(),
                access_token: user.into(),
            })),
        }
    }
}

fn model() -> AppModel {
    let analysis: Value = serde_json::from_str(include_str!(
        "../../rl-model/tests/fixtures/basic.analysis.json"
    ))
    .unwrap();
    let mut model = AppModel::from_analysis(&analysis).unwrap();
    let rule = |role: &str, actions: Vec<Action>| StorageRule {
        path_pattern: "**".into(),
        role: role.into(),
        actions,
        check: None,
    };
    model.storage.as_mut().unwrap().folders.push(StorageFolder {
        name: "public-notes".into(),
        display_name: "PublicNotes".into(),
        on_conflict: OnConflict::Overwrite,
        max_size: Some(16),
        allowed_content_types: vec!["text/*".into()],
        fields: vec![],
        rules: vec![
            rule("anonymous", vec![Action::Read]),
            rule(
                "authenticated",
                vec![Action::Create, Action::Update, Action::Delete],
            ),
        ],
    });
    model
}

struct Harness {
    router: Router,
    root: PathBuf,
    _dir: TempDir,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, String::from_utf8_lossy(&self.body))
    }
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap().to_owned()
    }
}

async fn harness() -> Harness {
    harness_with(model(), |_| {}).await
}

async fn harness_with(model: AppModel, adjust: impl FnOnce(&mut StorageConfig)) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("storage");
    let mut config = StorageConfig::new(&root, ORIGIN);
    adjust(&mut config);
    let db = Db::open_in_memory().unwrap();
    let router = rl_storage::router(config, db, Arc::new(model), Arc::new(Tokens))
        .await
        .unwrap();
    Harness {
        router,
        root,
        _dir: dir,
    }
}

/// `signed` with its last character changed.
fn tamper(signed: &str) -> String {
    let mut tampered = signed.to_owned();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    tampered
}

fn note(name: &str, prefix: Option<&str>) -> Value {
    json!({
        "folder": "public-notes", "fileName": name, "prefix": prefix, "contentType": "text/plain",
    })
}

impl Harness {
    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: body.collect().await.unwrap().to_bytes(),
        }
    }

    /// A service leg, as the client sends it.
    async fn api(&self, method: &str, uri: &str, user: Option<&str>, body: Option<Value>) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-correlation-id", "cid-1");
        if let Some(user) = user {
            request = request.header("authorization", format!("Bearer {user}"));
        }
        let body = match body {
            Some(json) => {
                request = request.header("content-type", "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        self.send(request.body(body).unwrap()).await
    }

    /// A signed-URL leg: no credentials, the URL used verbatim.
    async fn signed(&self, method: &str, url: &str, content_type: &str, bytes: &[u8]) -> Reply {
        let path = url.strip_prefix(ORIGIN).expect("signed URLs are absolute");
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", content_type)
            .header("x-ms-blob-type", "BlockBlob")
            .body(Body::from(bytes.to_vec()))
            .unwrap();
        self.send(request).await
    }

    /// A signed `GET` with extra request headers, as a browser's media element sends it.
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Reply {
        let path = url.strip_prefix(ORIGIN).expect("signed URLs are absolute");
        let mut request = Request::builder().uri(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }

    async fn poll(&self, user: Option<&str>, id: &str) -> Value {
        let reply = self
            .api(
                "GET",
                &format!("/api/storage/operations/{id}/status"),
                user,
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.json());
        reply.json()
    }

    /// Starts an upload; its operation id and signed URL.
    async fn start(&self, user: Option<&str>, request: Value) -> Result<(String, String), Reply> {
        let reply = self
            .api("POST", "/api/storage/upload", user, Some(request))
            .await;
        if reply.status != StatusCode::ACCEPTED {
            return Err(reply);
        }
        let id = reply.json()["operationId"].as_str().unwrap().to_owned();
        let poll = self.poll(user, &id).await;
        assert_eq!(poll["status"], "AwaitingUpload");
        Ok((id, poll["signedUrl"].as_str().unwrap().to_owned()))
    }

    /// The client's whole upload: start, poll, PUT, commit, poll until `Succeeded`.
    async fn upload(&self, user: Option<&str>, request: Value, bytes: &[u8]) -> Result<(), Reply> {
        let content_type = request["contentType"].as_str().unwrap().to_owned();
        let (id, url) = self.start(user, request).await?;
        let put = self.signed("PUT", &url, &content_type, bytes).await;
        if put.status != StatusCode::CREATED {
            return Err(put);
        }
        let commit_uri = format!("/api/storage/uploads/{id}/commit");
        let commit = self.api("POST", &commit_uri, user, None).await;
        if commit.status != StatusCode::ACCEPTED {
            return Err(commit);
        }
        assert_eq!(self.poll(user, &id).await["status"], "Succeeded");
        Ok(())
    }

    async fn list(&self, user: Option<&str>, query: &str) -> Reply {
        self.api("GET", &format!("/api/storage/list?{query}"), user, None)
            .await
    }

    async fn paths(&self, user: Option<&str>, query: &str) -> Vec<String> {
        let reply = self.list(user, query).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.json());
        let items = reply.json()["items"].as_array().unwrap().clone();
        items
            .iter()
            .map(|item| item["path"].as_str().unwrap().to_owned())
            .collect()
    }

    async fn delete(&self, user: Option<&str>, folder: &str, name: &str) -> Reply {
        let body = json!({ "folder": folder, "fileName": name });
        self.api("DELETE", "/api/storage/delete", user, Some(body))
            .await
    }

    fn files(&self, dir: &str) -> Vec<String> {
        std::fs::read_dir(self.root.join(dir))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }
}

#[tokio::test]
async fn upload_list_download_and_delete_like_the_client() {
    let h = harness().await;
    let alice = Some("alice");
    let request = json!({
        "folder": "todo-attachments", "fileName": "scan.pdf", "prefix": "team/2024",
        "contentType": "application/pdf", "contentDisposition": "attachment",
        "maxBytes": 11, "fields": { "team_id": "t1", "pages": 3 },
    });
    let start = h
        .api("POST", "/api/storage/upload", alice, Some(request))
        .await;
    assert_eq!(start.status, StatusCode::ACCEPTED);
    assert_eq!(start.headers["x-correlation-id"], "cid-1");
    assert_eq!(start.headers["cache-control"], "no-store");
    let id = start.json()["operationId"].as_str().unwrap().to_owned();

    let poll = h.poll(alice, &id).await;
    assert_eq!(poll["status"], "AwaitingUpload");
    assert!(poll["expiresAt"].as_str().unwrap().ends_with('Z'));
    let url = poll["signedUrl"].as_str().unwrap();
    assert!(
        url.starts_with("http://localhost:7171/.rayfin-local/storage/u/"),
        "{url}"
    );
    let put = h
        .signed("PUT", url, "application/pdf", b"hello world")
        .await;
    assert_eq!(put.status, StatusCode::CREATED);
    assert_eq!(put.headers["etag"], format!("\"{HELLO_SHA256}\""));
    let commit = h
        .api(
            "POST",
            &format!("/api/storage/uploads/{id}/commit"),
            alice,
            None,
        )
        .await;
    assert_eq!(commit.status, StatusCode::ACCEPTED);
    assert_eq!(commit.json()["operationId"], id.as_str());
    let done = h.poll(alice, &id).await;
    assert_eq!(
        (done["status"].clone(), done["success"].clone()),
        (json!("Succeeded"), json!(true))
    );

    let list = h.list(alice, "folder=todo-attachments&prefix=team").await;
    let items = list.json()["items"].clone();
    assert_eq!(items.as_array().unwrap().len(), 1);
    let item = &items[0];
    assert_eq!(item["path"], "team/2024/scan.pdf");
    assert_eq!(item["name"], "scan.pdf");
    assert_eq!(item["size"], 11);
    assert_eq!(item["contentType"], "application/pdf");
    assert_eq!(item["contentDisposition"], "attachment");
    assert_eq!(item["fields"], json!({ "team_id": "t1", "pages": 3 }));
    assert_eq!(item["etag"], format!("\"{HELLO_SHA256}\""));
    assert!(list.json().get("continuation").is_none());
    // The read rule is `claims.sub eq item.owner_id`: other users see nothing.
    assert!(
        h.paths(Some("bob"), "folder=todo-attachments")
            .await
            .is_empty()
    );

    let file =
        json!({ "folder": "todo-attachments", "fileName": "scan.pdf", "prefix": "team/2024" });
    let bob = h
        .api(
            "POST",
            "/api/storage/download",
            Some("bob"),
            Some(file.clone()),
        )
        .await;
    assert_eq!(
        (bob.status, bob.code()),
        (StatusCode::FORBIDDEN, "PermissionDenied".into())
    );
    let download = h
        .api("POST", "/api/storage/download", alice, Some(file.clone()))
        .await;
    assert_eq!(download.status, StatusCode::OK);
    let url = download.json()["signedUrl"].as_str().unwrap().to_owned();
    let get = h.signed("GET", &url, "", b"").await;
    assert_eq!(get.status, StatusCode::OK);
    assert_eq!(&get.body[..], b"hello world");
    assert_eq!(get.headers["content-type"], "application/pdf");
    assert_eq!(get.headers["content-length"], "11");
    assert_eq!(get.headers["content-disposition"], "attachment");
    assert_eq!(get.headers["etag"], format!("\"{HELLO_SHA256}\""));
    assert_eq!(get.headers["x-content-type-options"], "nosniff");

    let delete = h
        .api("DELETE", "/api/storage/delete", alice, Some(file))
        .await;
    assert_eq!(delete.status, StatusCode::ACCEPTED);
    let id = delete.json()["operationId"].as_str().unwrap().to_owned();
    assert_eq!(h.poll(alice, &id).await["status"], "Succeeded");
    assert!(h.paths(alice, "folder=todo-attachments").await.is_empty());
    assert_eq!(
        h.signed("GET", &url, "", b"").await.status,
        StatusCode::NOT_FOUND
    );
    assert!(h.files("objects").is_empty());
    // Operations belong to their caller.
    let status_uri = format!("/api/storage/operations/{id}/status");
    let foreign = h.api("GET", &status_uri, Some("bob"), None).await;
    assert_eq!(foreign.code(), "NotFound");
}

#[tokio::test]
async fn abort_drops_the_staged_upload() {
    let h = harness().await;
    let alice = Some("alice");
    let avatar =
        json!({ "folder": "user-avatars", "fileName": "me.png", "contentType": "image/png" });
    let (id, url) = h.start(alice, avatar.clone()).await.unwrap();
    assert_eq!(
        h.signed("PUT", &url, "image/png", b"png").await.status,
        StatusCode::CREATED
    );
    assert_eq!(h.files("staging").len(), 1);
    let abort = h
        .api("DELETE", &format!("/api/storage/uploads/{id}"), alice, None)
        .await;
    assert_eq!(abort.status, StatusCode::NO_CONTENT);
    assert!(h.files("staging").is_empty());
    let commit = h
        .api(
            "POST",
            &format!("/api/storage/uploads/{id}/commit"),
            alice,
            None,
        )
        .await;
    assert_eq!(
        (commit.status, commit.code()),
        (StatusCode::CONFLICT, "Conflict".into())
    );
    assert_eq!(h.poll(alice, &id).await["status"], "Failed");

    // Aborted before the PUT: the signed URL is dead.
    let (id, url) = h.start(alice, avatar.clone()).await.unwrap();
    h.api("DELETE", &format!("/api/storage/uploads/{id}"), alice, None)
        .await;
    assert_eq!(
        h.signed("PUT", &url, "image/png", b"png").await.status,
        StatusCode::FORBIDDEN
    );

    // A commit without the bytes is a conflict.
    let (id, _) = h.start(alice, avatar).await.unwrap();
    let commit = h
        .api(
            "POST",
            &format!("/api/storage/uploads/{id}/commit"),
            alice,
            None,
        )
        .await;
    assert_eq!(commit.status, StatusCode::CONFLICT);
    assert!(h.paths(alice, "folder=user-avatars").await.is_empty());
}

#[tokio::test]
async fn signed_urls_are_single_purpose_tamper_proof_and_expire() {
    let h = harness_with(model(), |config| config.url_ttl = Duration::from_secs(1)).await;
    let alice = Some("alice");
    let (_, url) = h.start(alice, note("a.txt", None)).await.unwrap();
    let mut tampered = url.clone();
    let last = tampered.pop().unwrap();
    tampered.push(if last == 'A' { 'B' } else { 'A' });
    assert_eq!(
        h.signed("PUT", &tampered, "text/plain", b"x").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.signed("GET", &url, "", b"").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let as_download = url.replace("/storage/u/", "/storage/o/");
    assert_eq!(
        h.signed("GET", &as_download, "", b"").await.status,
        StatusCode::FORBIDDEN
    );
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let expired = h.signed("PUT", &url, "text/plain", b"x").await;
    assert_eq!(expired.status, StatusCode::FORBIDDEN);
    assert_eq!(expired.json()["error"]["code"], "UploadSessionExpired");

    // An upload URL is used up by its first successful PUT.
    let mut request = note("b.txt", None);
    request["expiresInSeconds"] = 60.into();
    let (_, url) = h.start(alice, request).await.unwrap();
    assert_eq!(
        h.signed("PUT", &url, "text/plain", b"x").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        h.signed("PUT", &url, "text/plain", b"x").await.status,
        StatusCode::FORBIDDEN
    );
    let mut short = note("c.txt", None);
    short["expiresInSeconds"] = 30.into();
    let reply = h
        .api("POST", "/api/storage/upload", alice, Some(short))
        .await;
    assert_eq!(
        (reply.status, reply.code()),
        (StatusCode::BAD_REQUEST, "InvalidSasExpiry".into())
    );
}

#[tokio::test]
async fn sizes_and_content_types_follow_the_folder() {
    let h = harness().await;
    let alice = Some("alice");
    let big = json!({
        "folder": "todo-attachments", "fileName": "a.pdf", "contentType": "application/pdf",
        "maxBytes": 6 * 1024 * 1024,
    });
    let reply = h.api("POST", "/api/storage/upload", alice, Some(big)).await;
    assert_eq!(
        (reply.status, reply.code()),
        (StatusCode::BAD_REQUEST, "TooLarge".into())
    );
    let text =
        json!({ "folder": "todo-attachments", "fileName": "a.txt", "contentType": "text/plain" });
    let reply = h
        .api("POST", "/api/storage/upload", alice, Some(text))
        .await;
    assert_eq!(reply.code(), "UnsupportedContentType");
    let undeclared = json!({
        "folder": "todo-attachments", "fileName": "a.png", "contentType": "image/png",
        "fields": { "owner_id": "bob" },
    });
    let reply = h
        .api("POST", "/api/storage/upload", alice, Some(undeclared))
        .await;
    assert_eq!(
        (reply.status, reply.code()),
        (StatusCode::BAD_REQUEST, "Unexpected".into())
    );

    // A stream (no maxBytes) is held to the folder's maxSize (16) while it arrives; a failed PUT
    // can be retried with the same URL.
    let (_, url) = h.start(alice, note("n.txt", None)).await.unwrap();
    let put = h.signed("PUT", &url, "text/plain", &[b'x'; 17]).await;
    assert_eq!(put.status, StatusCode::PAYLOAD_TOO_LARGE);
    let put = h.signed("PUT", &url, "text/html", b"hi").await;
    assert_eq!(put.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        h.signed("PUT", &url, "text/plain; charset=utf-8", b"hi")
            .await
            .status,
        StatusCode::CREATED
    );
    assert!(
        h.files("staging")
            .iter()
            .all(|name| !name.ends_with(".part"))
    );

    let mut declared = note("d.txt", None);
    declared["maxBytes"] = 2.into();
    let (_, url) = h.start(alice, declared).await.unwrap();
    assert_eq!(
        h.signed("PUT", &url, "text/plain", b"abc").await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );

    let small = harness_with(model(), |config| config.max_total_bytes = 10).await;
    let mut over_quota = note("q.txt", None);
    over_quota["maxBytes"] = 11.into();
    let reply = small
        .api("POST", "/api/storage/upload", alice, Some(over_quota))
        .await;
    assert_eq!(reply.code(), "TooLarge");
}

#[tokio::test]
async fn commits_keep_uploads_staged_together_within_the_quota() {
    let h = harness_with(model(), |config| config.max_total_bytes = 10).await;
    let alice = Some("alice");
    // Both PUTs see the same 10 free bytes; only the first commit still fits.
    let (first, first_url) = h.start(alice, note("a.txt", None)).await.unwrap();
    let (second, second_url) = h.start(alice, note("b.txt", None)).await.unwrap();
    for url in [&first_url, &second_url] {
        let put = h.signed("PUT", url, "text/plain", &[b'x'; 8]).await;
        assert_eq!(put.status, StatusCode::CREATED);
    }
    let commit = |id: &str| format!("/api/storage/uploads/{id}/commit");
    let reply = h.api("POST", &commit(&first), alice, None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let reply = h.api("POST", &commit(&second), alice, None).await;
    assert_eq!(
        (reply.status, reply.code()),
        (StatusCode::BAD_REQUEST, "TooLarge".into())
    );
    let poll = h.poll(alice, &second).await;
    assert_eq!(
        (poll["status"].clone(), poll["errorCode"].clone()),
        (json!("Failed"), json!("UPLOAD_SIZE_EXCEEDS_LIMIT"))
    );
    assert!(h.files("staging").is_empty());
    assert_eq!(h.paths(alice, "folder=public-notes").await, ["a.txt"]);
    // An overwrite frees the bytes of the version it replaces.
    h.upload(alice, note("a.txt", None), &[b'y'; 9])
        .await
        .unwrap();
}

#[tokio::test]
async fn a_cancelled_put_leaves_no_partial_file() {
    let h = harness().await;
    let (_, url) = h
        .start(Some("alice"), note("slow.txt", None))
        .await
        .unwrap();
    // The client sends three bytes, then goes away.
    let sent = futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"abc"))]);
    let request = Request::builder()
        .method("PUT")
        .uri(url.strip_prefix(ORIGIN).unwrap())
        .header("content-type", "text/plain")
        .body(Body::from_stream(
            sent.chain(futures_util::stream::pending()),
        ))
        .unwrap();
    let put = tokio::spawn(h.router.clone().oneshot(request));
    // Not `DirEntry::metadata`: Windows lists the size of an open file as of its last close.
    let written = || {
        std::fs::read_dir(h.root.join("staging"))
            .unwrap()
            .flatten()
            .any(|entry| std::fs::metadata(entry.path()).is_ok_and(|meta| meta.len() == 3))
    };
    for _ in 0..500 {
        if written() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(written(), "the PUT never wrote its bytes");
    put.abort();
    assert!(put.await.unwrap_err().is_cancelled());
    assert!(h.files("staging").is_empty(), "{:?}", h.files("staging"));
}

#[tokio::test]
async fn signed_downloads_serve_a_single_byte_range() {
    let h = harness().await;
    h.upload(Some("alice"), note("r.txt", None), b"hello world")
        .await
        .unwrap();
    let download = h
        .api(
            "POST",
            "/api/storage/download",
            None,
            Some(note("r.txt", None)),
        )
        .await;
    let url = download.json()["signedUrl"].as_str().unwrap().to_owned();
    let etag = format!("\"{HELLO_SHA256}\"");

    for (range, bytes, content_range) in [
        ("bytes=0-4", "hello", "bytes 0-4/11"),
        ("bytes=4-99", "o world", "bytes 4-10/11"),
        ("bytes=6-", "world", "bytes 6-10/11"),
        ("bytes=-5", "world", "bytes 6-10/11"),
        ("bytes=-99", "hello world", "bytes 0-10/11"),
    ] {
        let get = h
            .get(&url, &[("range", range), ("if-range", etag.as_str())])
            .await;
        assert_eq!(get.status, StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(&get.body[..], bytes.as_bytes(), "{range}");
        assert_eq!(get.headers["content-range"], content_range);
        assert_eq!(get.headers["content-length"], bytes.len().to_string());
        assert_eq!(get.headers["accept-ranges"], "bytes");
        assert_eq!(get.headers["content-type"], "text/plain");
        assert_eq!(get.headers["etag"], etag);
        assert_eq!(get.headers["x-content-type-options"], "nosniff");
    }
    for range in ["bytes=11-", "bytes=-0"] {
        let get = h.get(&url, &[("range", range)]).await;
        assert_eq!(get.status, StatusCode::RANGE_NOT_SATISFIABLE, "{range}");
        assert_eq!(get.headers["content-range"], "bytes */11");
    }
    // No range, several, a malformed one, another unit or another version's `If-Range`: all of
    // the object.
    for headers in [
        vec![],
        vec![("range", "bytes=0-1,4-5")],
        vec![("range", "bytes=4-2")],
        vec![("range", "lines=0-1")],
        vec![("range", "bytes=0-4"), ("if-range", "\"other\"")],
    ] {
        let get = h.get(&url, &headers).await;
        assert_eq!(get.status, StatusCode::OK, "{headers:?}");
        assert_eq!(&get.body[..], b"hello world");
        assert_eq!(get.headers["content-length"], "11");
        assert_eq!(get.headers["accept-ranges"], "bytes");
    }
}

#[tokio::test]
async fn rules_follow_roles_owners_and_checks() {
    let h = harness().await;
    let (alice, bob) = (Some("alice"), Some("bob"));
    h.upload(alice, note("hello.txt", None), b"hi")
        .await
        .unwrap();
    // Anonymous rules let anyone read; writing needs a session.
    assert_eq!(h.paths(None, "folder=public-notes").await, ["hello.txt"]);
    let anonymous = h
        .api(
            "POST",
            "/api/storage/upload",
            None,
            Some(note("x.txt", None)),
        )
        .await;
    assert_eq!(
        (anonymous.status, anonymous.code()),
        (StatusCode::UNAUTHORIZED, "Unauthenticated".into())
    );
    let invalid = h.list(Some("bad"), "folder=public-notes").await;
    assert_eq!(invalid.code(), "Unauthenticated");
    assert_eq!(
        h.list(None, "folder=user-avatars").await.status,
        StatusCode::UNAUTHORIZED
    );

    // Avatars: any user reads, only the owner deletes, and onConflict is 'error'.
    let avatar =
        json!({ "folder": "user-avatars", "fileName": "alice.png", "contentType": "image/png" });
    h.upload(alice, avatar.clone(), b"png").await.unwrap();
    assert_eq!(h.paths(bob, "folder=user-avatars").await, ["alice.png"]);
    assert_eq!(
        h.delete(bob, "user-avatars", "alice.png").await.code(),
        "PermissionDenied"
    );
    let again = h.upload(alice, avatar, b"png2").await.unwrap_err();
    assert_eq!(
        (again.status, again.code()),
        (StatusCode::CONFLICT, "Conflict".into())
    );

    // Attachments overwrite, but only for the owner (`update` checks `owner_id`), who stays the
    // owner; deleting also needs a `team_id`.
    let attachment =
        json!({ "folder": "todo-attachments", "fileName": "a.png", "contentType": "image/png" });
    h.upload(alice, attachment.clone(), b"v1").await.unwrap();
    let denied = h.upload(bob, attachment.clone(), b"v2").await.unwrap_err();
    assert_eq!(denied.code(), "PermissionDenied");
    h.upload(alice, attachment, b"v3").await.unwrap();
    let list = h.list(alice, "folder=todo-attachments").await.json();
    assert_eq!(list["items"][0]["size"], 2);
    assert_eq!(
        h.delete(alice, "todo-attachments", "a.png").await.code(),
        "PermissionDenied"
    );
    assert_eq!(
        h.files("objects").len(),
        3,
        "the overwritten version's file is gone"
    );
}

#[tokio::test]
async fn names_are_validated_and_never_reach_the_disk() {
    let h = harness().await;
    let alice = Some("alice");
    for (name, prefix, code) in [
        ("..", None, "InvalidName"),
        ("a/b", None, "InvalidName"),
        ("a\\b", None, "InvalidName"),
        ("a\u{0}b", None, "InvalidName"),
        ("a.txt", Some("../etc"), "InvalidPrefix"),
        ("a.txt", Some("/abs"), "InvalidPrefix"),
    ] {
        let reply = h
            .api(
                "POST",
                "/api/storage/upload",
                alice,
                Some(note(name, prefix)),
            )
            .await;
        assert_eq!(
            (reply.status, reply.code()),
            (StatusCode::BAD_REQUEST, code.into()),
            "{name:?} {prefix:?}"
        );
    }
    let unknown = json!({ "folder": "nope", "fileName": "a.txt", "contentType": "text/plain" });
    let reply = h
        .api("POST", "/api/storage/upload", alice, Some(unknown))
        .await;
    assert_eq!(
        (reply.status, reply.code()),
        (StatusCode::NOT_FOUND, "NotFound".into())
    );
    let reply = h.list(alice, "prefix=a").await;
    assert_eq!(reply.code(), "InvalidFolder");

    h.upload(alice, note("..notes.txt", Some("x/y")), b"hi")
        .await
        .unwrap();
    let files = h.files("objects");
    assert_eq!(files.len(), 1);
    assert!(files[0].len() == 32 && files[0].bytes().all(|b| b.is_ascii_hexdigit()));
}

#[tokio::test]
async fn lists_page_through_prefixes_with_continuations() {
    let h = harness().await;
    let alice = Some("alice");
    for (name, prefix) in [
        ("1.txt", Some("p")),
        ("2.txt", Some("p")),
        ("3.txt", Some("p")),
        ("4.txt", Some("p/sub")),
        ("q.txt", None),
        ("5.txt", Some("pp")),
    ] {
        h.upload(alice, note(name, prefix), b"x").await.unwrap();
    }
    let first = h
        .list(None, "folder=public-notes&prefix=p&limit=2")
        .await
        .json();
    let paths: Vec<_> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].clone())
        .collect();
    assert_eq!(paths, ["p/1.txt", "p/2.txt"]);
    let token = first["continuation"].as_str().unwrap();
    let query = format!("folder=public-notes&prefix=p&limit=2&continuation={token}");
    let second = h.list(None, &query).await.json();
    let paths: Vec<_> = second["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].clone())
        .collect();
    assert_eq!(paths, ["p/3.txt", "p/sub/4.txt"]);
    assert!(second.get("continuation").is_none());
    assert_eq!(h.paths(None, "folder=public-notes&limit=0").await.len(), 6);

    let forged = format!("folder=public-notes&continuation={}", tamper(token));
    let reply = h.list(None, &forged).await;
    assert_eq!(
        (reply.status, reply.code()),
        (StatusCode::BAD_REQUEST, "InvalidContinuation".into())
    );
}

#[tokio::test]
async fn errors_carry_the_correlation_id() {
    let h = harness().await;
    let reply = h.api("GET", "/api/storage/nope", Some("alice"), None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.headers["x-correlation-id"], "cid-1");
    assert_eq!(reply.json()["error"]["code"], "NotFound");
    assert_eq!(reply.json()["error"]["correlationId"], "cid-1");

    let mut bare = model();
    bare.storage = None;
    let h = harness_with(bare, |_| {}).await;
    let reply = h
        .api(
            "POST",
            "/api/storage/upload",
            Some("alice"),
            Some(note("a.txt", None)),
        )
        .await;
    assert_eq!(
        (reply.status, reply.code()),
        (
            StatusCode::BAD_REQUEST,
            "NoStorageEntitiesConfigured".into()
        )
    );
}
