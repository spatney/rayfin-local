//! How `serve` configures each feature service from the project, and the services not wired yet.
//!
//! Each feature returns its router (state applied) for `rl_server::build_router`, or `None` when
//! it is off. Features that start processes register them in [`StopHooks`].

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use axum::Router;
use rl_auth::{AuthService, AuthSettings};
use rl_connectors::{ConnectorDef, ConnectorsConfig, DEFAULT_POWERBI_BASE_URL};
use rl_db::Db;
use rl_model::{AppModel, CallerResolver};
use rl_node::NodeRuntime;
use rl_project::config::{ConnectorAuthType, ConnectorEntry};
use rl_project::{LocalState, Project, TokenSourceKind};
use rl_server::StaticSite;
use rl_tokens::{
    Audience, RayfinCliConfig, TokenBroker, TokenBrokerConfig, TokenMode, TokenSource,
};

use crate::commands::serve::ServeOptions;
use crate::shutdown::StopHooks;
use crate::ui;

/// What the feature services are built from.
#[allow(
    dead_code,
    reason = "the GraphQL, functions, storage and frontend integrations read the rest"
)]
pub struct ServeContext<'a> {
    pub project: &'a Project,
    pub state: &'a LocalState,
    pub node: &'a NodeRuntime,
    pub model: &'a Arc<AppModel>,
    pub db: &'a Db,
    pub auth: &'a Arc<AuthService>,
    pub broker: &'a Arc<TokenBroker>,
    pub options: &'a ServeOptions,
    /// The interface the backend listens on.
    pub host: &'a str,
    /// Where the backend is reached, such as `http://localhost:7171`.
    pub api_origin: &'a str,
    /// The dev server's port (`None` with `--no-frontend` or `--static`).
    pub frontend_port: Option<u16>,
}

impl ServeContext<'_> {
    /// Who a request's bearer token belongs to.
    pub fn callers(&self) -> Arc<dyn CallerResolver> {
        self.auth.clone()
    }

    /// The developer's Azure tokens.
    pub fn tokens(&self) -> Arc<dyn TokenSource> {
        self.broker.clone()
    }
}

/// Brings the entity tables up to date with the model. Changes that would lose data fail unless
/// `force` (`--force-schema`).
pub async fn apply_schema(db: &Db, model: &AppModel, force: bool) -> anyhow::Result<()> {
    let report = rl_db::apply(db, model, force)
        .await
        .context("Could not update the database to the app's entities")?;
    for warning in &report.warnings {
        ui::warn(warning);
    }
    for loss in &report.forced {
        ui::warn(format_args!("--force-schema: {loss}"));
    }
    if !report.steps.is_empty() {
        for step in &report.steps {
            tracing::debug!("schema: {step}");
        }
        let count = report.steps.len();
        ui::step(format_args!(
            "Updated the database schema ({count} {})",
            if count == 1 { "change" } else { "changes" }
        ));
    }
    Ok(())
}

/// The token broker: `--token-source`/`--tenant`, else `[tokens]` in `rayfin-local.toml`.
/// Without `node`, only the Azure CLI (and env overrides) are used.
pub fn token_broker(
    project: &Project,
    state: &LocalState,
    node: Option<&NodeRuntime>,
    mode: Option<TokenMode>,
    tenant: Option<String>,
) -> anyhow::Result<TokenBroker> {
    let tokens = &project.local_config().tokens;
    let mode = mode.or(tokens.source.map(token_mode)).unwrap_or_default();
    let tenant = tenant
        .or_else(|| tokens.tenant.clone())
        .map(|tenant| tenant.trim().to_owned())
        .filter(|tenant| !tenant.is_empty());
    Ok(TokenBroker::new(TokenBrokerConfig {
        mode,
        tenant,
        scope_overrides: scope_overrides(&tokens.scopes)?,
        rayfin: node.map(|node| RayfinCliConfig {
            node: node.clone(),
            project_root: project.root().to_path_buf(),
            cache_dir: state.cache_dir(),
        }),
        ..Default::default()
    }))
}

fn token_mode(kind: TokenSourceKind) -> TokenMode {
    match kind {
        TokenSourceKind::Auto => TokenMode::Auto,
        TokenSourceKind::Az => TokenMode::Az,
        TokenSourceKind::Rayfin => TokenMode::Rayfin,
    }
}

/// `[tokens.scopes]`: audience name → scope.
fn scope_overrides(scopes: &BTreeMap<String, String>) -> anyhow::Result<HashMap<Audience, String>> {
    scopes
        .iter()
        .map(|(name, scope)| {
            let audience = Audience::from_udf_name(name).ok_or_else(|| {
                anyhow!(
                    "Unknown audience `{name}` in [tokens.scopes] of rayfin-local.toml. Use one \
                     of: {}.",
                    Audience::ALL.map(Audience::udf_name).join(", ")
                )
            })?;
            Ok((audience, scope.trim().to_owned()))
        })
        .collect()
}

/// `services.auth` of `rayfin.yml`. Signing in with the developer's Entra identity is on only
/// when the backend listens on loopback.
pub fn auth_settings(project: &Project, app_id: &str, loopback: bool) -> AuthSettings {
    let auth = &project.config().services.auth;
    let mut settings = AuthSettings::new(app_id);
    settings.enabled = auth.enabled;
    settings.access_token_minutes = auth.access_token_lifetime_minutes();
    settings.refresh_token_days = auth.refresh_token_lifetime_days();
    settings.session_absolute_lifetime_minutes = auth.refresh_token_absolute_lifetime_minutes();
    settings.password_enabled = auth.password_enabled();
    settings.fabric_enabled = auth.fabric_enabled();
    settings.external_entra_exchange = auth.external_entra_exchange_enabled();
    settings.email_enabled = auth
        .email
        .as_ref()
        .and_then(|email| email.enabled)
        .unwrap_or(false);
    settings.magic_link_enabled = auth.magic_link_enabled();
    settings.allowed_redirect_uris = auth.allowed_redirect_uris.clone();
    settings.custom_claims = auth
        .custom_claims
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    settings.scopes = auth.scopes.clone();
    settings.app_name = Some(project.display_name().to_owned());
    settings.entra_signin.enabled = loopback;
    settings
}

/// Whether `host` only accepts connections from this machine.
pub fn is_loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// `POST /connector-invoke/<name>`: the `connectors:` of `rayfin.yml`, queried with the
/// developer's Power BI token.
pub fn connectors_router(ctx: &ServeContext<'_>) -> Router {
    let powerbi_base_url = ctx
        .project
        .local_config()
        .connectors
        .powerbi_base_url
        .as_deref()
        .map(|url| url.trim().trim_end_matches('/'))
        .filter(|url| !url.is_empty())
        .unwrap_or(DEFAULT_POWERBI_BASE_URL);
    let config = ConnectorsConfig {
        connectors: connector_defs(&ctx.project.config().connectors),
        powerbi_base_url: powerbi_base_url.to_owned(),
        powerbi_scope: ctx.broker.scope_for(Audience::PowerBi),
    };
    rl_connectors::router(config, ctx.callers(), ctx.tokens())
}

/// `rayfin.yml` `connectors:` as `rl-connectors` takes them. Declaring `all` allows every
/// operation, which `rl-connectors` spells as no operations.
pub fn connector_defs(entries: &[ConnectorEntry]) -> Vec<ConnectorDef> {
    entries
        .iter()
        .map(|entry| {
            let all = entry
                .operations
                .iter()
                .any(|op| op.name.trim().eq_ignore_ascii_case("all"));
            ConnectorDef {
                name: entry.name.clone(),
                kind: entry.connector_type.clone(),
                version: entry.version.clone(),
                config: entry
                    .config
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
                auth_type: entry.auth_type().map(|auth| {
                    match auth {
                        ConnectorAuthType::Delegated => "delegated",
                        ConnectorAuthType::Application => "application",
                    }
                    .to_owned()
                }),
                operations: if all {
                    Vec::new()
                } else {
                    entry.operations.iter().map(|op| op.name.clone()).collect()
                },
            }
        })
        .collect()
}

/// `POST /graphql`.
pub fn graphql_router(ctx: &ServeContext<'_>) -> anyhow::Result<Option<Router>> {
    if !ctx.project.config().services.data.enabled {
        return Ok(None);
    }
    tracing::debug!("the GraphQL data API is not wired yet");
    Ok(None)
}

/// `/api/storage/*`.
pub fn storage_router(ctx: &ServeContext<'_>) -> anyhow::Result<Option<Router>> {
    if !ctx.project.config().services.storage.enabled {
        return Ok(None);
    }
    tracing::debug!("storage is not wired yet");
    Ok(None)
}

/// `/functions/<name>/invoke`, served by the function host this starts.
pub async fn functions_router(
    ctx: &ServeContext<'_>,
    hooks: &mut StopHooks,
) -> anyhow::Result<Option<Router>> {
    let _ = hooks;
    if ctx.options.no_functions || !ctx.project.config().services.functions.enabled {
        return Ok(None);
    }
    let inspect_port = ctx
        .options
        .inspect
        .or(ctx.project.local_config().functions.inspect_port);
    tracing::debug!(?inspect_port, "functions are not wired yet");
    Ok(None)
}

/// The built app the backend serves with `--static`.
pub async fn static_site(ctx: &ServeContext<'_>) -> anyhow::Result<Option<StaticSite>> {
    if !ctx.options.static_build {
        return Ok(None);
    }
    tracing::debug!("the static frontend build is not wired yet");
    Ok(None)
}

/// Starts the app's dev server; returns its URL.
pub async fn start_frontend(
    ctx: &ServeContext<'_>,
    hooks: &mut StopHooks,
) -> anyhow::Result<Option<String>> {
    let _ = hooks;
    let Some(port) = ctx.frontend_port else {
        return Ok(None);
    };
    tracing::debug!(port, "the frontend dev server is not wired yet");
    Ok(None)
}

#[cfg(test)]
mod tests {
    use rl_project::RayfinConfig;

    use super::*;

    #[test]
    fn connectors_map_from_rayfin_yml() {
        let yaml = r#"
connectors:
  - name: sales
    type: fabric-semanticmodel
    version: 1
    config:
      workspaceId: 11111111-1111-1111-1111-111111111111
      itemId: 22222222-2222-2222-2222-222222222222
    auth:
      type: delegated
    operations:
      - name: executeQuery
  - name: everything
    type: kusto
    operations:
      - name: all
"#;
        let config = RayfinConfig::parse(
            yaml,
            &BTreeMap::new(),
            std::path::Path::new("rayfin.yml"),
            std::path::Path::new(".env"),
        )
        .unwrap();
        let defs = connector_defs(&config.connectors);
        assert_eq!(defs.len(), 2);
        let sales = &defs[0];
        assert_eq!(sales.name, "sales");
        assert_eq!(sales.kind, "fabric-semanticmodel");
        assert_eq!(sales.version.as_deref(), Some("1"));
        assert_eq!(
            sales.config["itemId"],
            "22222222-2222-2222-2222-222222222222"
        );
        assert_eq!(sales.auth_type.as_deref(), Some("delegated"));
        assert_eq!(sales.operations, ["executeQuery"]);
        let everything = &defs[1];
        assert_eq!(everything.auth_type, None);
        assert!(everything.operations.is_empty());
    }

    #[test]
    fn unknown_scope_audiences_are_errors() {
        let scopes = BTreeMap::from([("AzureAI".to_owned(), " api://x/.default ".to_owned())]);
        let overrides = scope_overrides(&scopes).unwrap();
        assert_eq!(overrides[&Audience::AzureAi], "api://x/.default");
        let scopes = BTreeMap::from([("Graph".to_owned(), "x".to_owned())]);
        let err = scope_overrides(&scopes).unwrap_err();
        assert!(err.to_string().contains("`Graph`"), "{err}");
    }

    #[test]
    fn loopback_hosts() {
        for host in ["127.0.0.1", "localhost", "::1", "[::1]", "127.0.0.2"] {
            assert!(is_loopback(host), "{host}");
        }
        for host in ["0.0.0.0", "::", "192.168.1.10", "example.com"] {
            assert!(!is_loopback(host), "{host}");
        }
    }
}
