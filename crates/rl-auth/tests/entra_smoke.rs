//! The Entra exchange with a real token: mints the developer's Power BI token with the Azure CLI
//! and exchanges it at `POST /api/auth/v1/brokered/token` on a bound server, which checks it
//! against Microsoft's published signing keys; then asks the sign-in page who "Continue as" would
//! sign in. Prints no token and no identity.
//!
//! Ignored by default: it needs `az login` and the network. Run with
//! `cargo test -p rl-auth --test entra_smoke -- --ignored --nocapture`.

use std::sync::Arc;

use rl_auth::{AuthService, AuthSettings, PORTAL_PATH};
use rl_db::Db;
use rl_model::CallerResolver;
use rl_tokens::{Audience, TokenBroker, TokenBrokerConfig, TokenMode, TokenSource};
use serde_json::{Value, json};

const APP_ID: &str = "6c6f6361-6c00-4000-8000-0000000000e5";

#[tokio::test]
#[ignore = "needs `az login` and the network (see the module docs)"]
async fn a_real_power_bi_token_signs_in() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let broker = Arc::new(TokenBroker::new(TokenBrokerConfig {
        mode: TokenMode::Az,
        ..TokenBrokerConfig::default()
    }));
    let token = match broker.token(Audience::PowerBi.default_scope()).await {
        Ok(token) => token,
        Err(error) => {
            println!(
                "SKIPPED: no Power BI token from az: {error} ({:?})",
                error.hint
            );
            return;
        }
    };

    let dir = tempfile::tempdir().unwrap();
    let mut settings = AuthSettings::new(APP_ID);
    settings.entra_signin.enabled = true;
    let auth = AuthService::new(
        Db::open_in_memory().unwrap(),
        dir.path().join("keys"),
        settings,
    )
    .await
    .unwrap()
    .with_entra(broker);
    let auth = Arc::new(auth);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let router = Arc::clone(&auth).router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();

    let exchange = |token: String| {
        client
            .post(format!("{api}/api/auth/v1/brokered/token"))
            .bearer_auth(token)
            .send()
    };
    let response = exchange(token.token.clone()).await.unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    println!("exchange: HTTP {status}, error: {}", body["error"]);
    assert_eq!(status, 200, "{}", body["message"]);
    assert_eq!(body["tokenType"], "Bearer");
    assert!(body["expiresIn"].as_u64().unwrap() > 0);
    assert!(body["refreshToken"].is_string());
    let session = auth
        .resolve(body["accessToken"].as_str())
        .unwrap()
        .session
        .unwrap();
    let users = auth.list_users().await.unwrap();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].id, session.user_id);
    assert!(users[0].entra_oid.is_some());
    println!(
        "session: has email {}, has name {}",
        session.email.is_some(),
        session.claims.contains_key("name")
    );

    // A tampered token is refused.
    let tampered = format!("{}x", token.token);
    assert_eq!(exchange(tampered).await.unwrap().status(), 401);

    // "Continue as": the page's whoami, as the page calls it.
    let page = client
        .get(format!("{api}{PORTAL_PATH}/secureItemEmbed"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let config_start = page.find(r#"id="rl-config">"#).unwrap() + r#"id="rl-config">"#.len();
    let config: Value = serde_json::from_str(
        &page[config_start..][..page[config_start..].find("</script>").unwrap()],
    )
    .unwrap();
    assert_eq!(config["entra"], true);
    let whoami: Value = client
        .post(format!("{api}{PORTAL_PATH}/api/whoami"))
        .header("origin", &api)
        .header("x-rayfin-local-csrf", config["csrf"].as_str().unwrap())
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    println!(
        "whoami: available {}, has name {}",
        whoami["available"],
        whoami["name"].is_string()
    );
    assert_eq!(whoami["available"], true, "{}", whoami["message"]);
    server.abort();
}
