//! The Rayfin CLI provider (`js/rayfin-token.mjs`), against projects with a fake
//! `@microsoft/rayfin-cli`.

mod common;

use std::{
    path::Path,
    time::{Duration, SystemTime},
};

use common::FakeAz;
use rl_tokens::{
    Audience, RayfinCliConfig, TokenBroker, TokenBrokerConfig, TokenMode, TokenSource,
};
use serde_json::{Value, json};

/// A fake `@microsoft/rayfin-cli/auth` without a cached sign-in: silent acquisition fails, and
/// the "interactive" one succeeds. Calls are recorded in `<project>/rayfin-calls.jsonl`.
const AUTH_MODULE: &str = r#"
import { appendFileSync } from 'node:fs';

const record = (call) => appendFileSync('rayfin-calls.jsonl', `${JSON.stringify(call)}\n`);

export async function ensureAuthenticated(scopes, options) {
  record({ fn: 'ensureAuthenticated', scopes, options });
  throw new Error('Silent token acquisition failed and interactive login was not allowed');
}

export async function getAuthenticatedToken(scopes, options) {
  record({ fn: 'getAuthenticatedToken', scopes, options });
  return {
    token: `rayfin-token-for-${scopes[0]}`,
    expiresOnTimestamp: 1900000000000,
    tenantId: options?.tenantId ?? 'home-tenant',
    identityType: 'user',
  };
}
"#;

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    /// A project; with `auth_module`, it has `@microsoft/rayfin-cli` with that `./auth` export.
    fn new(auth_module: Option<&str>) -> Self {
        Self::with_auth_export(auth_module, "1.36.2", "./auth", "./dist/auth/index.js")
    }

    fn with_auth_export(
        auth_module: Option<&str>,
        version: &str,
        subpath: &str,
        target: &str,
    ) -> Self {
        let dir = common::scratch_dir();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"app","private":true}"#,
        );
        if let Some(source) = auth_module {
            let package = dir.path().join("node_modules/@microsoft/rayfin-cli");
            let manifest = json!({
                "name": "@microsoft/rayfin-cli",
                "version": version,
                "type": "module",
                "exports": {
                    ".": { "import": "./dist/index.js", "types": "./dist/index.d.ts" },
                    (subpath): { "import": target },
                    "./package.json": "./package.json"
                }
            });
            write(&package.join("package.json"), &manifest.to_string());
            write(&package.join(target), source);
        }
        Self { dir }
    }

    async fn rayfin(&self) -> RayfinCliConfig {
        RayfinCliConfig {
            node: rl_node::NodeRuntime::discover().await.unwrap(),
            project_root: self.dir.path().into(),
            cache_dir: self.dir.path().join(".rayfin-local/cache"),
        }
    }

    async fn broker(&self, mode: TokenMode, az: Option<&FakeAz>) -> TokenBroker {
        TokenBroker::new(TokenBrokerConfig {
            mode,
            tenant: Some("contoso.onmicrosoft.com".into()),
            rayfin: Some(self.rayfin().await),
            az_program: az.map(|az| az.program("not-signed-in")),
            ..TokenBrokerConfig::default()
        })
    }

    /// A broker whose Azure CLI is signed in.
    async fn broker_with_az(&self, mode: TokenMode, az: &FakeAz) -> TokenBroker {
        TokenBroker::new(TokenBrokerConfig {
            mode,
            tenant: Some("contoso.onmicrosoft.com".into()),
            rayfin: Some(self.rayfin().await),
            az_program: Some(az.program("ok")),
            ..TokenBrokerConfig::default()
        })
    }

    /// The fake package's functions called, in order.
    fn called(&self) -> Vec<String> {
        self.calls()
            .iter()
            .map(|call| call["fn"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The fake package's calls, in order.
    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("rayfin-calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[tokio::test]
async fn signs_in_with_the_projects_rayfin_cli() {
    let project = Project::new(Some(AUTH_MODULE));
    let broker = project.broker(TokenMode::Rayfin, None).await;
    let scope = Audience::Ado.default_scope();

    let token = broker.token(scope).await.unwrap();
    assert_eq!(token.token, format!("rayfin-token-for-{scope}"));
    assert_eq!(
        token.expires_at,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_900_000_000)
    );
    let options = json!({ "tenantId": "contoso.onmicrosoft.com" });
    assert_eq!(
        project.calls(),
        [
            json!({
                "fn": "ensureAuthenticated",
                "scopes": [scope],
                "options": { "tenantId": "contoso.onmicrosoft.com", "silent": true }
            }),
            json!({ "fn": "getAuthenticatedToken", "scopes": [scope], "options": options }),
        ]
    );
}

#[tokio::test]
async fn loads_the_v2_internal_auth_export() {
    let project = Project::with_auth_export(
        Some(AUTH_MODULE),
        "2.0.0",
        "./_internal/auth",
        "./internal-token.mjs",
    );
    let broker = project.broker(TokenMode::Rayfin, None).await;
    let scope = Audience::Fabric.default_scope();
    let token = broker.token(scope).await.unwrap();
    assert_eq!(token.token, format!("rayfin-token-for-{scope}"));
    assert_eq!(
        project.called(),
        ["ensureAuthenticated", "getAuthenticatedToken"]
    );
}

#[tokio::test]
async fn a_project_without_the_rayfin_cli_gets_an_install_hint() {
    let project = Project::new(None);
    let broker = project.broker(TokenMode::Rayfin, None).await;
    let error = broker
        .token(Audience::Ado.default_scope())
        .await
        .unwrap_err();
    assert!(
        error
            .message
            .starts_with("@microsoft/rayfin-cli is not installed"),
        "{}",
        error.message
    );
    assert!(
        error
            .hint
            .unwrap()
            .contains("npm i -D @microsoft/rayfin-cli")
    );
}

#[tokio::test]
async fn silent_tokens_never_sign_in() {
    let az = FakeAz::new();
    let project = Project::new(Some(AUTH_MODULE));
    let broker = project.broker(TokenMode::Auto, Some(&az)).await;
    let scope = Audience::PowerBi.default_scope();
    let silent_call = json!({
        "fn": "ensureAuthenticated",
        "scopes": [scope],
        "options": { "tenantId": "contoso.onmicrosoft.com", "silent": true }
    });

    let error = broker.token_silently(scope).await.unwrap_err();
    assert!(
        error.message.contains("without a prompt"),
        "{}",
        error.message
    );
    assert_eq!(project.calls(), std::slice::from_ref(&silent_call));
    assert_eq!(az.calls().len(), 1);

    // What a sign-in got is served from the cache, silently.
    let token = broker.token(scope).await.unwrap();
    assert_eq!(broker.token_silently(scope).await.unwrap(), token);
    assert_eq!(project.calls().len(), 3);

    let signed_in = Project::new(Some(&AUTH_MODULE.replace(
        "throw new Error('Silent token acquisition failed and interactive login was not allowed');",
        "return { token: `cached-token-for-${scopes[0]}`, expiresOnTimestamp: 1900000000000 };",
    )));
    let broker = signed_in.broker(TokenMode::Rayfin, None).await;
    let token = broker.token_silently(scope).await.unwrap();
    assert_eq!(token.token, format!("cached-token-for-{scope}"));
    assert_eq!(signed_in.calls(), [silent_call]);
}

#[tokio::test]
async fn auto_mode_falls_back_to_the_rayfin_cli() {
    let az = FakeAz::new();
    let project = Project::new(Some(AUTH_MODULE));
    let broker = project.broker(TokenMode::Auto, Some(&az)).await;
    let scope = Audience::Fabric.default_scope();
    assert_eq!(
        broker.token(scope).await.unwrap().token,
        format!("rayfin-token-for-{scope}")
    );
    assert_eq!(az.calls().len(), 1);
}

#[tokio::test]
async fn auto_mode_reports_both_failures() {
    let az = FakeAz::new();
    let project = Project::new(None);
    let broker = project.broker(TokenMode::Auto, Some(&az)).await;
    let error = broker
        .token(Audience::Fabric.default_scope())
        .await
        .unwrap_err();
    assert!(
        error
            .message
            .contains("Azure CLI: the Azure CLI is not signed in"),
        "{}",
        error.message
    );
    assert!(
        error
            .message
            .contains("Rayfin CLI: @microsoft/rayfin-cli is not installed"),
        "{}",
        error.message
    );
    let hint = error.hint.unwrap();
    assert!(
        hint.contains("az login --tenant contoso.onmicrosoft.com"),
        "{hint}"
    );
    assert!(hint.contains("npm i -D @microsoft/rayfin-cli"), "{hint}");
}

#[tokio::test]
async fn a_refused_azure_cli_token_is_replaced_by_the_rayfin_clis() {
    let az = FakeAz::new();
    let project = Project::new(Some(AUTH_MODULE));
    let broker = project.broker_with_az(TokenMode::Auto, &az).await;
    let scope = Audience::PowerBi.default_scope();
    let refused = broker.token(scope).await.unwrap();

    // Requests whose token was refused share one sign-in, which may prompt.
    let (first, second) = tokio::join!(
        broker.another_token(scope, &refused.token),
        broker.another_token(scope, &refused.token)
    );
    let other = first.unwrap().unwrap();
    assert_eq!(other.token, format!("rayfin-token-for-{scope}"));
    assert_eq!(second, Some(Ok(other.clone())));
    assert_eq!(
        project.called(),
        ["ensureAuthenticated", "getAuthenticatedToken"]
    );

    assert_eq!(broker.token(scope).await.unwrap(), other);
    assert_eq!(
        broker.another_token(scope, &refused.token).await,
        Some(Ok(other.clone()))
    );
    // The Rayfin CLI's own token has no other sign-in to try.
    assert_eq!(broker.another_token(scope, &other.token).await, None);
    assert_eq!(project.calls().len(), 2);
    assert_eq!(az.calls().len(), 1);

    let az_only = project.broker_with_az(TokenMode::Az, &az).await;
    let token = az_only.token(scope).await.unwrap();
    assert_eq!(az_only.another_token(scope, &token.token).await, None);
}

#[tokio::test]
async fn a_scope_whose_azure_cli_token_was_refused_asks_the_rayfin_cli_first() {
    let az = FakeAz::new();
    // Tokens about to expire, so every request fetches one.
    let project = Project::new(Some(&AUTH_MODULE.replace(
        "expiresOnTimestamp: 1900000000000",
        "expiresOnTimestamp: Date.now() + 60_000",
    )));
    let broker = project.broker_with_az(TokenMode::Auto, &az).await;
    let scope = Audience::PowerBi.default_scope();
    let refused = broker.token(scope).await.unwrap();
    broker
        .another_token(scope, &refused.token)
        .await
        .unwrap()
        .unwrap();

    let token = broker.token(scope).await.unwrap();
    assert_eq!(token.token, format!("rayfin-token-for-{scope}"));
    assert_eq!(az.calls().len(), 1);
    broker
        .token(Audience::Fabric.default_scope())
        .await
        .unwrap();
    assert_eq!(az.calls().len(), 2);
}

#[tokio::test]
async fn only_the_first_replacement_may_prompt() {
    let az = FakeAz::new();
    let project = Project::new(Some(&AUTH_MODULE.replace(
        "record({ fn: 'getAuthenticatedToken', scopes, options });",
        "record({ fn: 'getAuthenticatedToken', scopes, options });\n  \
         throw new Error('the developer closed the sign-in window');",
    )));
    let broker = project.broker_with_az(TokenMode::Auto, &az).await;
    let scope = Audience::PowerBi.default_scope();
    let refused = broker.token(scope).await.unwrap();

    let error = broker
        .another_token(scope, &refused.token)
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .message
            .contains("the developer closed the sign-in window"),
        "{}",
        error.message
    );
    assert_eq!(broker.token(scope).await.unwrap(), refused);

    let error = broker
        .another_token(scope, &refused.token)
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error.message.contains("without a prompt"),
        "{}",
        error.message
    );
    assert_eq!(
        project.called(),
        [
            "ensureAuthenticated",
            "getAuthenticatedToken",
            "ensureAuthenticated"
        ]
    );
}
