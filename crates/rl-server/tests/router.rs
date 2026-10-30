//! The composed router: runtime config, publishable key, CORS, Host check, static site, 404s.

use std::path::Path;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::routing::{get, post};
use http_body_util::BodyExt;
use rl_server::{ServerConfig, StaticSite, build_router};
use tower::ServiceExt;

const KEY: &str = "pk-local-test";
const ORIGIN: &str = "http://localhost:7171";

fn config() -> ServerConfig {
    ServerConfig {
        api_origin: ORIGIN.to_owned(),
        publishable_key: KEY.to_owned(),
        workspace_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        item_id: "22222222-2222-4222-8222-222222222222".to_owned(),
        tenant_id: Some("33333333-3333-4333-8333-333333333333".to_owned()),
        portal_url: format!("{ORIGIN}/.rayfin-local/portal"),
        allowed_origins: vec!["https://App.Contoso.example/auth/callback".to_owned()],
        allowed_hosts: None,
        static_site: None,
        publishable_key_exempt: vec!["/api/projectRuntimeSettings".to_owned()],
    }
}

/// Stand-ins for the feature crates' routers.
fn features() -> Vec<Router> {
    vec![
        Router::new().route("/graphql", post(|| async { "graphql" })),
        Router::new()
            .route("/api/auth/v1/token", post(|| async { "token" }))
            .route(
                "/api/auth/v1/.well-known/jwks.json",
                get(|| async { "jwks alias" }),
            ),
        Router::new().route("/.well-known/jwks.json", get(|| async { "jwks" })),
        Router::new()
            .route(
                "/api/projectRuntimeSettings",
                get(|| async { "runtime settings" }),
            )
            .route("/api/projectRuntimeSettings/x", get(|| async { "nested" })),
        Router::new().route("/functions/{name}/invoke", post(|| async { "invoked" })),
        Router::new().route("/connector-invoke/{name}", post(|| async { "connector" })),
        Router::new().route(
            "/connectors/{name}/graphql",
            post(|| async { "category a" }),
        ),
        Router::new().nest(
            "/.rayfin-local/portal",
            Router::new().route("/secureItemEmbed", get(|| async { "portal" })),
        ),
    ]
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

async fn send(router: &Router, method: Method, uri: &str, headers: &[(&str, &str)]) -> Reply {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes(),
    }
}

#[tokio::test]
async fn runtime_config_has_the_sdk_shape_and_is_never_cached() {
    let router = build_router(config(), features());
    let reply = send(&router, Method::GET, "/rayfin.config.json", &[]).await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    let expected = [
        r#"{"apiUrl":"http://localhost:7171","publishableKey":"pk-local-test","#,
        r#""workspaceId":"11111111-1111-4111-8111-111111111111","#,
        r#""itemId":"22222222-2222-4222-8222-222222222222","#,
        r#""portalUrl":"http://localhost:7171/.rayfin-local/portal","#,
        r#""tenantId":"33333333-3333-4333-8333-333333333333"}"#,
    ];
    assert_eq!(reply.text(), expected.concat());

    let router = build_router(
        ServerConfig {
            api_origin: format!("{ORIGIN}/"),
            tenant_id: None,
            ..config()
        },
        vec![],
    );
    let json = send(&router, Method::GET, "/rayfin.config.json", &[])
        .await
        .json();
    assert_eq!(json["apiUrl"], ORIGIN);
    assert!(json.get("tenantId").is_none());
}

#[tokio::test]
async fn health_answers_ok() {
    let router = build_router(config(), vec![]);
    let reply = send(&router, Method::GET, rl_server::HEALTH_PATH, &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), serde_json::json!({"ok": true}));
}

#[tokio::test]
async fn sdk_routes_need_this_apps_publishable_key() {
    let router = build_router(config(), features());
    let routes = [
        (Method::POST, "/graphql"),
        (Method::POST, "/api/auth/v1/token"),
        (Method::POST, "/functions/hello/invoke"),
        (Method::POST, "/connector-invoke/sales"),
        (Method::POST, "/connectors/sales/graphql"),
        (Method::GET, "/api/no-such-route"),
    ];
    for (method, uri) in routes {
        for headers in [&[][..], &[("X-Publishable-Key", "pk-someone-else")][..]] {
            let reply = send(&router, method.clone(), uri, headers).await;
            assert_eq!(
                reply.status,
                StatusCode::FORBIDDEN,
                "{method} {uri} {headers:?}"
            );
            let json = reply.json();
            assert_eq!(json["error"], "invalid_publishable_key");
            let description = json["error_description"].as_str().unwrap();
            assert!(description.contains("X-Publishable-Key"), "{description}");
            assert_eq!(json["message"], json["error_description"]);
        }
    }

    let with_key = [("X-Publishable-Key", KEY)];
    let reply = send(&router, Method::POST, "/graphql", &with_key).await;
    assert_eq!((reply.status, reply.text()), (StatusCode::OK, "graphql"));
    let reply = send(&router, Method::POST, "/functions/hello/invoke", &with_key).await;
    assert_eq!((reply.status, reply.text()), (StatusCode::OK, "invoked"));
    let reply = send(&router, Method::GET, "/api/no-such-route", &with_key).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reject_anonymous_answers_like_a_tenant_without_anonymous_access() {
    let guarded = Router::new()
        .route("/graphql", post(|| async { "graphql" }))
        .layer(axum::middleware::from_fn(rl_server::reject_anonymous));
    let router = build_router(config(), vec![guarded]);
    let app = ("Origin", "http://localhost:5173");

    let reply = send(
        &router,
        Method::POST,
        "/graphql",
        &[("X-Publishable-Key", KEY), app],
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        reply.json(),
        serde_json::json!({ "error": rl_server::ANONYMOUS_ACCESS_DISABLED })
    );
    // The SDK can read the rejection.
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("http://localhost:5173")
    );
    for authorization in ["Bearer", "Bearer  ", "Basic abc"] {
        let headers = [("X-Publishable-Key", KEY), ("Authorization", authorization)];
        let reply = send(&router, Method::POST, "/graphql", &headers).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{authorization}");
    }

    let signed_in = [
        ("X-Publishable-Key", KEY),
        ("Authorization", "bearer t0k3n"),
    ];
    let reply = send(&router, Method::POST, "/graphql", &signed_in).await;
    assert_eq!((reply.status, reply.text()), (StatusCode::OK, "graphql"));
    // The publishable key is checked first, as on Fabric.
    let reply = send(&router, Method::POST, "/graphql", &[]).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn public_routes_need_no_key() {
    let router = build_router(config(), features());
    for (uri, body) in [
        ("/.well-known/jwks.json", "jwks"),
        ("/api/auth/v1/.well-known/jwks.json", "jwks alias"),
        ("/.rayfin-local/portal/secureItemEmbed", "portal"),
    ] {
        let reply = send(&router, Method::GET, uri, &[]).await;
        assert_eq!(
            (reply.status, reply.text()),
            (StatusCode::OK, body),
            "{uri}"
        );
    }
    let reply = send(&router, Method::GET, "/rayfin.config.json", &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn configured_exempt_paths_need_no_key_and_match_exactly() {
    let router = build_router(config(), features());
    let reply = send(&router, Method::GET, "/api/projectRuntimeSettings", &[]).await;
    assert_eq!(
        (reply.status, reply.text()),
        (StatusCode::OK, "runtime settings")
    );
    let reply = send(&router, Method::GET, "/api/projectRuntimeSettings/x", &[]).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn preflights_pass_without_a_key() {
    let router = build_router(config(), features());
    let requested = "x-publishable-key,authorization,content-type,x-ms-workload-resource-moniker,\
                     x-correlation-id,x-request-id,activityid";
    let reply = send(
        &router,
        Method::OPTIONS,
        "/graphql",
        &[
            ("Origin", "http://localhost:5175"),
            ("Access-Control-Request-Method", "POST"),
            ("Access-Control-Request-Headers", requested),
        ],
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("http://localhost:5175")
    );
    assert_eq!(
        reply.header("access-control-allow-headers"),
        Some(requested)
    );
    let methods = reply.header("access-control-allow-methods").unwrap();
    for method in ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
        assert!(methods.contains(method), "{methods}");
    }
    assert_eq!(reply.header("access-control-max-age"), Some("600"));
    assert_eq!(reply.header("access-control-allow-credentials"), None);
}

#[tokio::test]
async fn cors_allows_loopback_and_redirect_uri_origins_only() {
    let router = build_router(config(), features());
    for origin in [
        "http://localhost:5175",
        "http://127.0.0.1:3000",
        "http://[::1]:8080",
        "https://app.contoso.example",
    ] {
        let reply = send(
            &router,
            Method::GET,
            "/rayfin.config.json",
            &[("Origin", origin)],
        )
        .await;
        assert_eq!(reply.header("access-control-allow-origin"), Some(origin));
        let exposed = reply.header("access-control-expose-headers").unwrap();
        for name in [
            "x-correlation-id",
            "requestid",
            "x-fabric-udf-status",
            "retry-after",
            "content-disposition",
            "x-ms-request-id",
        ] {
            assert!(exposed.contains(name), "{exposed}");
        }
    }
    for origin in [
        "https://evil.example",
        "http://localhost.evil.example",
        "null",
    ] {
        let reply = send(
            &router,
            Method::GET,
            "/rayfin.config.json",
            &[("Origin", origin)],
        )
        .await;
        assert_eq!(
            reply.header("access-control-allow-origin"),
            None,
            "{origin}"
        );
        let preflight = send(
            &router,
            Method::OPTIONS,
            "/graphql",
            &[
                ("Origin", origin),
                ("Access-Control-Request-Method", "POST"),
            ],
        )
        .await;
        assert_eq!(
            preflight.header("access-control-allow-origin"),
            None,
            "{origin}"
        );
    }

    // A rejection still carries CORS headers, so the SDK can read its message.
    let origin = [("Origin", "http://localhost:5175")];
    let reply = send(&router, Method::POST, "/graphql", &origin).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(
        reply.header("access-control-allow-origin"),
        Some("http://localhost:5175")
    );
}

#[tokio::test]
async fn routes_for_programs_are_never_allowed_to_browsers() {
    let mut routes = features();
    routes.push(
        Router::new()
            .route("/.rayfin-local/control/status", get(|| async { "status" }))
            .route(
                "/.rayfin-local/dev/session-token",
                get(|| async { "token" }),
            ),
    );
    let router = build_router(config(), routes);
    for path in [
        "/.rayfin-local/control/status",
        "/.rayfin-local/dev/session-token",
    ] {
        let preflight = send(
            &router,
            Method::OPTIONS,
            path,
            &[
                ("Origin", "http://localhost:5175"),
                ("Access-Control-Request-Method", "GET"),
                ("Access-Control-Request-Headers", "authorization"),
            ],
        )
        .await;
        assert_eq!(
            preflight.header("access-control-allow-origin"),
            None,
            "{path}"
        );
        let reply = send(
            &router,
            Method::GET,
            path,
            &[("Origin", "http://localhost:5175")],
        )
        .await;
        assert_eq!(reply.header("access-control-allow-origin"), None, "{path}");
        // Without a publishable key: programs call them.
        assert_eq!(reply.status, StatusCode::OK, "{path}");
    }
    let portal = send(
        &router,
        Method::GET,
        "/.rayfin-local/portal/secureItemEmbed",
        &[("Origin", "http://localhost:5175")],
    )
    .await;
    assert_eq!(
        portal.header("access-control-allow-origin"),
        Some("http://localhost:5175")
    );
}

#[tokio::test]
async fn cors_allows_other_ports_of_the_allowed_host_a_request_is_addressed_to() {
    let router = build_router(
        ServerConfig {
            allowed_hosts: Some(vec!["192.168.1.20".to_owned()]),
            ..config()
        },
        features(),
    );
    // `serve --host 192.168.1.20`: the dev server's page calls the backend on the same address.
    let lan_app = "http://192.168.1.20:5173";
    let reply = send(
        &router,
        Method::GET,
        "/rayfin.config.json",
        &[("Origin", lan_app), ("Host", "192.168.1.20:7171")],
    )
    .await;
    assert_eq!(reply.header("access-control-allow-origin"), Some(lan_app));
    let preflight = send(
        &router,
        Method::OPTIONS,
        "/graphql",
        &[
            ("Origin", lan_app),
            ("Host", "192.168.1.20:7171"),
            ("Access-Control-Request-Method", "POST"),
        ],
    )
    .await;
    assert_eq!(
        preflight.header("access-control-allow-origin"),
        Some(lan_app)
    );

    // Another machine's origin, or a host the Host check refuses, gets no CORS headers.
    for (origin, host) in [
        ("http://192.168.1.21:5173", "192.168.1.20:7171"),
        ("http://evil.example:5173", "evil.example:7171"),
    ] {
        let reply = send(
            &router,
            Method::GET,
            "/rayfin.config.json",
            &[("Origin", origin), ("Host", host)],
        )
        .await;
        assert_eq!(
            reply.header("access-control-allow-origin"),
            None,
            "{origin}"
        );
    }

    // Without the Host check (wildcard addresses), the request's host proves nothing.
    let unchecked = build_router(config(), features());
    let reply = send(
        &unchecked,
        Method::GET,
        "/rayfin.config.json",
        &[("Origin", lan_app), ("Host", "192.168.1.20:7171")],
    )
    .await;
    assert_eq!(reply.header("access-control-allow-origin"), None);
}

#[tokio::test]
async fn host_check_rejects_foreign_hosts() {
    let router = build_router(
        ServerConfig {
            allowed_hosts: Some(vec!["myapp.test".to_owned()]),
            ..config()
        },
        features(),
    );
    for host in [
        "localhost:7171",
        "127.0.0.1:7171",
        "[::1]:7171",
        "myapp.test:7171",
        "MyApp.Test",
    ] {
        let reply = send(
            &router,
            Method::GET,
            "/rayfin.config.json",
            &[("Host", host)],
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{host}");
    }
    for host in [
        "evil.example:7171",
        "localhost.evil.example",
        "192.168.1.20:7171",
    ] {
        let reply = send(
            &router,
            Method::GET,
            "/rayfin.config.json",
            &[("Host", host)],
        )
        .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{host}");
        let json = reply.json();
        assert_eq!(json["error"], "host_not_allowed");
        assert!(json["message"].as_str().unwrap().contains(ORIGIN), "{json}");
    }
    let reply = send(&router, Method::GET, "/rayfin.config.json", &[]).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "no Host header");

    let unchecked = build_router(config(), features());
    let evil = [("Host", "evil.example")];
    let reply = send(&unchecked, Method::GET, "/rayfin.config.json", &evil).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn unknown_routes_get_a_json_404() {
    let router = build_router(config(), features());
    for uri in ["/", "/threads/42", "/.rayfin-local/nothing"] {
        let reply = send(&router, Method::GET, uri, &[("Accept", "text/html")]).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
        let json = reply.json();
        assert_eq!(json["error"], "not_found");
        assert!(json["message"].as_str().unwrap().contains(uri), "{json}");
    }
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

const INDEX: &str = "<!doctype html><title>app</title>";
const SECRET: &str = "TOP SECRET";

/// A static site in `<dir>/dist`, with a file outside it at `<dir>/secret.txt`.
fn static_router(dir: &Path) -> Router {
    let root = dir.join("dist");
    write(&root.join("index.html"), INDEX);
    write(&root.join("assets/app.js"), "console.log('app')");
    write(&root.join("assets/app.css"), "body{}");
    write(&root.join("logo.svg"), "<svg/>");
    write(&dir.join("secret.txt"), SECRET);
    let site = StaticSite {
        root,
        index_document: "index.html".to_owned(),
    };
    build_router(
        ServerConfig {
            static_site: Some(site),
            ..config()
        },
        features(),
    )
}

#[tokio::test]
async fn static_files_have_their_mime_type_and_no_cache() {
    let dir = tempfile::tempdir().unwrap();
    let router = static_router(dir.path());
    for (uri, mime, body) in [
        ("/assets/app.js", "text/javascript", "console.log('app')"),
        ("/assets/app.css", "text/css", "body{}"),
        ("/logo.svg", "image/svg+xml", "<svg/>"),
        ("/index.html", "text/html", INDEX),
    ] {
        let reply = send(&router, Method::GET, uri, &[("Accept", "*/*")]).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert_eq!(reply.header("content-type"), Some(mime), "{uri}");
        assert_eq!(reply.header("cache-control"), Some("no-cache"), "{uri}");
        assert_eq!(reply.text(), body);
    }
    let reply = send(&router, Method::HEAD, "/assets/app.js", &[]).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("text/javascript"));
    assert!(reply.body.is_empty());
}

#[tokio::test]
async fn page_navigations_fall_back_to_the_index_document() {
    let dir = tempfile::tempdir().unwrap();
    let router = static_router(dir.path());
    let html = [("Accept", "text/html,application/xhtml+xml,*/*;q=0.8")];
    for (uri, headers) in [
        ("/", &html[..]),
        ("/threads/42", &html[..]),
        ("/threads/42", &[][..]),
        ("/assets", &html[..]),
        ("/functions-overview", &html[..]),
    ] {
        let reply = send(&router, Method::GET, uri, headers).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert_eq!(reply.header("content-type"), Some("text/html"), "{uri}");
        assert_eq!(reply.header("cache-control"), Some("no-cache"), "{uri}");
        assert_eq!(reply.text(), INDEX, "{uri}");
    }

    let not_pages = [
        (Method::GET, "/assets/missing.js", "*/*"),
        (Method::GET, "/threads/42", "application/json"),
        (Method::POST, "/threads/42", "text/html"),
        (Method::GET, "/api/x", "text/html"),
        (Method::GET, "/graphql", "text/html"),
        (Method::GET, "/functions", "text/html"),
        (Method::GET, "/connectors/x", "text/html"),
        (Method::GET, "/.well-known/x", "text/html"),
        (Method::GET, "/.rayfin-local/x", "text/html"),
    ];
    for (method, uri, accept) in not_pages {
        let headers = [("Accept", accept), ("X-Publishable-Key", KEY)];
        let reply = send(&router, method.clone(), uri, &headers).await;
        assert!(
            reply.status.is_client_error(),
            "{method} {uri}: {}",
            reply.status
        );
        assert_ne!(reply.text(), INDEX, "{method} {uri}");
    }
    let reply = send(
        &router,
        Method::GET,
        "/api/x",
        &[("X-Publishable-Key", KEY)],
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.json()["error"], "not_found");
}

#[tokio::test]
async fn static_paths_cannot_leave_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let router = static_router(dir.path());
    let absolute = dir.path().join("secret.txt");
    let absolute = format!(
        "/{}",
        absolute
            .to_str()
            .unwrap()
            .replace('\\', "/")
            .replace(' ', "%20")
            .trim_start_matches('/')
    );
    for uri in [
        "/../secret.txt",
        "/assets/../../secret.txt",
        "/%2e%2e/secret.txt",
        "/assets/..%2f..%2fsecret.txt",
        "/..%5csecret.txt",
        "/assets/..%5c..%5csecret.txt",
        "/%2e%2e%5csecret.txt",
        &absolute,
    ] {
        for accept in ["*/*", "text/html"] {
            let reply = send(&router, Method::GET, uri, &[("Accept", accept)]).await;
            assert!(!reply.text().contains(SECRET), "{uri} leaked the file");
        }
    }
}
