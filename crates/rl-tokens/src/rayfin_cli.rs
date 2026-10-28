//! The Rayfin CLI provider: the developer's `rayfin login`, through the project's own
//! `@microsoft/rayfin-cli/auth` (`getAuthenticatedToken`), run by `js/rayfin-token.mjs`.

use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::json;

use crate::{
    AccessToken, RayfinCliConfig, TokenError,
    az::{aadsts_code, failure, shorten},
    jwt,
};

const HELPER: rl_node::Helper =
    rl_node::Helper::new("rayfin-token", include_str!("../../../js/rayfin-token.mjs"));

/// How long the helper may take. When the CLI's cache can't serve a scope silently, it signs in
/// interactively (a browser window, or a device code in the log) and waits for the developer.
const TIMEOUT: Duration = Duration::from_secs(180);

/// How the helper reports a project without the package (`js/rayfin-token.mjs`).
const NOT_INSTALLED: &str = "@microsoft/rayfin-cli is not installed";

pub(crate) struct RayfinCli {
    config: RayfinCliConfig,
    /// One helper at a time: after one interactive sign-in, the other scopes come silently from
    /// the refreshed cache instead of opening a browser window each.
    turn: tokio::sync::Mutex<()>,
}

/// The helper's result.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HelperToken {
    token: String,
    /// Milliseconds since the epoch.
    #[serde(default)]
    expires_on_timestamp: Option<f64>,
    #[serde(default)]
    tenant_id: Option<String>,
}

impl RayfinCli {
    pub(crate) fn new(config: RayfinCliConfig) -> Self {
        Self {
            config,
            turn: tokio::sync::Mutex::new(()),
        }
    }

    pub(crate) async fn token(
        &self,
        scope: &str,
        tenant: Option<&str>,
    ) -> Result<AccessToken, TokenError> {
        let _turn = self.turn.lock().await;
        let mut input = json!({ "scopes": [scope] });
        if let Some(tenant) = tenant {
            input["tenantId"] = json!(tenant);
        }
        tracing::debug!(
            scope,
            project = %self.config.project_root.display(),
            "getting a token with the Rayfin CLI's login (it may open a browser to sign in)"
        );
        let run = rl_node::run_helper::<HelperToken>(
            &self.config.node,
            &HELPER,
            &self.config.cache_dir,
            &self.config.project_root,
            &input,
            rl_node::default_sink(),
        );
        let result = match tokio::time::timeout(TIMEOUT, run).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => return Err(helper_failure(scope, error)),
            Err(_) => {
                return Err(failure(
                    scope,
                    format!(
                        "the Rayfin CLI's sign-in did not finish within {} s",
                        TIMEOUT.as_secs()
                    ),
                    "Finish the sign-in in the browser window it opened, or sign in first with \
                     `npx rayfin login` in the project; or use the Azure CLI (`--token-source az`)."
                        .to_owned(),
                ));
            }
        };
        let token = result.token.trim().to_owned();
        if token.is_empty() {
            return Err(failure(
                scope,
                "the Rayfin CLI's login returned an empty token".to_owned(),
                "Sign in again with `npx rayfin login` in the project.".to_owned(),
            ));
        }
        let expires_at = result
            .expires_on_timestamp
            .filter(|ms| *ms > 0.0 && *ms < 1e14)
            .map(|ms| SystemTime::UNIX_EPOCH + Duration::from_secs_f64(ms / 1000.0))
            .or_else(|| jwt::expiry(&token))
            // Unknown expiry: short enough not to hand out a stale token for long.
            .unwrap_or_else(|| SystemTime::now() + Duration::from_secs(5 * 60));
        tracing::debug!(
            scope,
            tenant = result.tenant_id.as_deref().unwrap_or("unknown"),
            "the Rayfin CLI's login returned a token"
        );
        Ok(AccessToken { token, expires_at })
    }
}

fn helper_failure(scope: &str, error: rl_node::NodeError) -> TokenError {
    match error {
        rl_node::NodeError::Helper { message, .. } if message.contains(NOT_INSTALLED) => failure(
            scope,
            message,
            "Install @microsoft/rayfin-cli in the project (`npm i -D @microsoft/rayfin-cli`) and \
             sign in with `npx rayfin login`, or use the Azure CLI (`az login`)."
                .to_owned(),
        ),
        rl_node::NodeError::Helper { message, .. } => {
            let hint = if aadsts_code(&message).as_deref() == Some("65001") {
                "The Rayfin CLI app is not consented for this resource in your tenant: use the \
                 Azure CLI instead (`--token-source az`), or override this audience's scope in \
                 rayfin-local.toml."
            } else {
                "Sign in with `npx rayfin login` in the project, then retry; or use the Azure CLI \
                 (`az login`)."
            };
            failure(
                scope,
                format!(
                    "the Rayfin CLI's login could not get a token for {scope}: {}",
                    shorten(&message)
                ),
                hint.to_owned(),
            )
        }
        other => failure(
            scope,
            format!("could not run the Rayfin CLI token helper: {other}"),
            "Check that Node.js works (`node --version`), or use the Azure CLI \
             (`--token-source az`)."
                .to_owned(),
        ),
    }
}
