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
        let dir = common::scratch_dir();
        write(
            &dir.path().join("package.json"),
            r#"{"name":"app","private":true}"#,
        );
        if let Some(source) = auth_module {
            let package = dir.path().join("node_modules/@microsoft/rayfin-cli");
            let manifest = json!({
                "name": "@microsoft/rayfin-cli",
                "version": "1.36.2",
                "type": "module",
                "exports": {
                    ".": { "import": "./dist/index.js", "types": "./dist/index.d.ts" },
                    "./auth": { "import": "./dist/auth/index.js", "types": "./dist/auth/index.d.ts" },
                    "./package.json": "./package.json"
                }
            });
            write(&package.join("package.json"), &manifest.to_string());
            write(&package.join("dist/auth/index.js"), source);
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
