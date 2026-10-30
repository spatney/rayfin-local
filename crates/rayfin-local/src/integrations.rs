//! How `serve` configures each feature service from the project.
//!
//! Each feature returns its router (state applied) for `rl_server::build_router`, or `None` when
//! it is off. Features that start processes register them in [`StopHooks`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use axum::Router;
use ring::rand::{SecureRandom as _, SystemRandom};
use rl_auth::{AuthService, AuthSettings};
use rl_connectors::{
    ConnectorDef, ConnectorsConfig, DEFAULT_POWERBI_BASE_URL, SqlConnectorsConfig,
};
use rl_db::Db;
use rl_frontend::{DevServer, DevServerConfig, RayfinEnv, StaticBuildConfig};
use rl_functions::{FunctionsConfig, FunctionsService};
use rl_graphql::{DataApi, DataApiSettings};
use rl_model::{AppModel, CallerResolver};
use rl_node::NodeRuntime;
use rl_project::config::{ConnectorAuthType, ConnectorEntry};
use rl_project::{FrontendFramework, LocalState, Project, TokenSourceKind, write_atomic};
use rl_server::StaticSite;
use rl_storage::StorageConfig;
use rl_tokens::{
    Audience, RayfinCliConfig, TokenBroker, TokenBrokerConfig, TokenMode, TokenSource,
};
use tokio::sync::oneshot;

use crate::analysis::SourcesStamp;
use crate::commands::serve::ServeOptions;
use crate::shutdown::StopHooks;
use crate::ui;

/// The file in `.rayfin-local/keys` with the key that signs storage URLs (hex), kept so that
/// signed URLs stay valid across restarts.
const STORAGE_KEY_FILE: &str = "storage-signing.key";

/// What the feature services are built from.
pub struct ServeContext<'a> {
    pub project: &'a Project,
    pub state: &'a LocalState,
    pub node: &'a NodeRuntime,
    pub model: &'a Arc<AppModel>,
    /// What `model` was analyzed from, for live reload to catch changes made since.
    pub model_stamp: &'a SourcesStamp,
    pub db: &'a Db,
    pub auth: &'a Arc<AuthService>,
    pub broker: &'a Arc<TokenBroker>,
    /// The GraphQL data API (`None` when `services.data` is off); live reload swaps its model.
    pub data_api: Option<Arc<DataApi>>,
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
    settings.auto_create_users = project
        .local_config()
        .auth
        .auto_create_users
        .unwrap_or(true);
    settings
}

/// Whether `host` only accepts connections from this machine.
pub fn is_loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// `POST /connector-invoke/<name>` and `POST /connectors/<name>/graphql`: the `connectors:` of
/// `rayfin.yml`, queried with the developer's Power BI token (semantic models) or over their
/// Fabric SQL source with the developer's Entra token (Category A connectors, whose entities are
/// analyzed here).
pub async fn connectors_router(ctx: &ServeContext<'_>) -> Router {
    let powerbi_base_url = ctx
        .project
        .local_config()
        .connectors
        .powerbi_base_url
        .as_deref()
        .map(|url| url.trim().trim_end_matches('/'))
        .filter(|url| !url.is_empty())
        .unwrap_or(DEFAULT_POWERBI_BASE_URL);
    let models =
        crate::connectors::analyze(ctx.project, ctx.state, ctx.node, ctx.options.verbose).await;
    let config = ConnectorsConfig {
        connectors: connector_defs(&ctx.project.config().connectors),
        powerbi_base_url: powerbi_base_url.to_owned(),
        powerbi_scope: ctx.broker.scope_for(Audience::PowerBi),
        sql: SqlConnectorsConfig {
            models,
            fabric_scope: ctx.broker.scope_for(Audience::Fabric),
            sql_scope: ctx.broker.scope_for(Audience::Sql),
            cache_dir: Some(ctx.state.cache_dir()),
            ..SqlConnectorsConfig::default()
        },
    };
    rl_connectors::router(config, ctx.callers(), ctx.tokens())
}

/// The banner's connectors: each with how it runs locally (`none` without connectors).
pub fn connectors_summary(entries: &[ConnectorEntry]) -> String {
    if entries.is_empty() {
        return "none".to_owned();
    }
    entries
        .iter()
        .map(|entry| {
            let kind = &entry.connector_type;
            if rl_connectors::runs_locally(kind) {
                format!("{} ({kind}, as you)", entry.name)
            } else {
                format!("{} ({kind}: Fabric only, answers 501)", entry.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
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

/// The GraphQL data API over the project database (`None` when `services.data` is off).
pub fn data_api(
    project: &Project,
    db: &Db,
    model: &Arc<AppModel>,
    callers: Arc<dyn CallerResolver>,
) -> Option<Arc<DataApi>> {
    let data = &project.config().services.data;
    if !data.enabled {
        return None;
    }
    let settings = DataApiSettings {
        allow_introspection: data.allow_introspection,
        ..DataApiSettings::default()
    };
    Some(Arc::new(DataApi::new(
        db.clone(),
        Arc::clone(model),
        callers,
        settings,
    )))
}

/// `POST /graphql`.
pub fn graphql_router(ctx: &ServeContext<'_>) -> Option<Router> {
    ctx.data_api.clone().map(DataApi::router)
}

/// `/api/storage/*`, when `services.storage` is on and `rayfin/storage` declares folders.
pub async fn storage_router(ctx: &ServeContext<'_>) -> anyhow::Result<Option<Router>> {
    if !ctx.project.config().services.storage.enabled || storage_folders(ctx.model).is_empty() {
        return Ok(None);
    }
    let mut config = StorageConfig::new(ctx.state.storage_dir(), ctx.api_origin);
    config.signing_key = signing_key(&ctx.state.keys_dir().join(STORAGE_KEY_FILE))?;
    let router = rl_storage::router(config, ctx.db.clone(), Arc::clone(ctx.model), ctx.callers())
        .await
        .context("Could not start the local storage service")?;
    Ok(Some(router))
}

/// The names of the storage folders in `model`.
pub fn storage_folders(model: &AppModel) -> Vec<&str> {
    model
        .storage
        .iter()
        .flat_map(|storage| &storage.folders)
        .map(|folder| folder.name.as_str())
        .collect()
}

/// The 32-byte key stored in `path` as hex; a new random one (saved there) when the file is
/// missing or holds no valid key.
fn signing_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    let mut key = [0; 32];
    match std::fs::read_to_string(path) {
        Ok(text) if hex::decode_to_slice(text.trim(), &mut key).is_ok() => return Ok(key),
        Ok(_) => ui::warn(format_args!(
            "{} does not hold a valid key; replacing it (signed storage URLs issued before stop \
             working).",
            path.display()
        )),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("Could not read {}", path.display()));
        }
    }
    SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| anyhow!("Could not generate a storage signing key: no system randomness"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Could not create {}", dir.display()))?;
    }
    write_atomic(path, hex::encode(key).as_bytes())
        .with_context(|| format!("Could not write {}", path.display()))?;
    Ok(key)
}

/// Builds the functions and starts their host (`None` with `--no-functions` or when
/// `services.functions` is off). A failed build still starts: invocations answer 503 until a
/// fixed build is seen.
pub async fn start_functions(
    ctx: &ServeContext<'_>,
    hooks: &mut StopHooks,
) -> anyhow::Result<Option<Arc<FunctionsService>>> {
    let functions = &ctx.project.config().services.functions;
    if ctx.options.no_functions || !functions.enabled {
        return Ok(None);
    }
    let data = ctx.state.data();
    let functions_dir = ctx.project.functions_dir();
    let mut config = FunctionsConfig::new(
        ctx.node.clone(),
        functions_dir,
        ctx.state.cache_dir(),
        ctx.api_origin,
        data.publishable_key.clone(),
    );
    config.build_command = functions
        .build_command
        .clone()
        .filter(|command| !command.trim().is_empty());
    config.workspace_id = data.workspace_id.clone();
    config.item_id = data.item_id.clone();
    config.secrets = function_secrets(ctx);
    config.inspect_port = ctx
        .options
        .inspect
        .or(ctx.project.local_config().functions.inspect_port);
    config.sink = ui::child_sink();
    let service = FunctionsService::start(config, ctx.callers(), ctx.tokens())
        .await
        .with_context(|| {
            format!(
                "Could not start the functions in {}",
                functions_dir.display()
            )
        })?;
    let host = Arc::clone(&service);
    hooks.add(
        "the function host",
        move || async move { host.stop().await },
    );
    Ok(Some(service))
}

/// The values of the secrets `rayfin.yml` declares: from the environment, else from
/// `.rayfin-local/secrets.json`. The function host reads `local.settings.json` itself; secrets
/// with no value in any of them are reported.
fn function_secrets(ctx: &ServeContext<'_>) -> Vec<(String, String)> {
    let declared = &ctx.project.config().secrets;
    if declared.is_empty() {
        return Vec::new();
    }
    let stored = ctx.state.load_secrets().unwrap_or_else(|err| {
        ui::warn(format_args!(
            "Could not read the local secret values: {err}"
        ));
        BTreeMap::new()
    });
    let functions_dir = ctx.project.functions_dir();
    let settings = match rl_functions::host::local_settings_values(functions_dir) {
        Ok(values) => values.into_iter().map(|(name, _)| name).collect(),
        Err(err) => {
            tracing::debug!("could not read local.settings.json: {err}");
            BTreeSet::new()
        }
    };
    let (values, missing) = resolve_secrets(
        declared.iter().map(|secret| secret.name.as_str()),
        |name| std::env::var(name).ok(),
        &stored,
        &settings,
    );
    let settings_file = functions_dir.join("local.settings.json");
    let settings_file = settings_file
        .strip_prefix(ctx.project.root())
        .unwrap_or(&settings_file)
        .to_string_lossy()
        .replace('\\', "/");
    for name in missing {
        ui::warn(format_args!(
            "secret {name} has no local value: add it to {settings_file} Values or run \
             `rayfin-local secret set {name}`"
        ));
    }
    values
}

/// Each named secret's value: `env`, else `stored`. Also returns the secrets with a value in
/// neither nor in `local.settings.json` (`settings`).
fn resolve_secrets<'a>(
    names: impl IntoIterator<Item = &'a str>,
    env: impl Fn(&str) -> Option<String>,
    stored: &BTreeMap<String, String>,
    settings: &BTreeSet<String>,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut values = Vec::new();
    let mut missing = Vec::new();
    let mut seen = BTreeSet::new();
    for name in names {
        let name = name.trim();
        if name.is_empty() || !seen.insert(name) {
            continue;
        }
        let value = env(name)
            .filter(|value| !value.is_empty())
            .or_else(|| stored.get(name).cloned());
        match value {
            Some(value) => values.push((name.to_owned(), value)),
            None if settings.contains(name) => {}
            None => missing.push(name.to_owned()),
        }
    }
    (values, missing)
}

/// What points the app's frontend at this backend.
fn rayfin_env(ctx: &ServeContext<'_>) -> RayfinEnv {
    let data = ctx.state.data();
    let public = ctx.project.env().public_env();
    RayfinEnv {
        api_origin: ctx.api_origin.to_owned(),
        functions_url: format!("{}/.rayfin-local/functions", ctx.api_origin),
        publishable_key: data.publishable_key.clone(),
        workspace_id: data.workspace_id.clone(),
        item_id: data.item_id.clone(),
        tenant_id: data.tenant_id.clone(),
        portal_url: format!("{}{}", ctx.api_origin, rl_auth::PORTAL_PATH),
        extra: rl_project::map_public_env(&public, FrontendFramework::Vite)
            .into_iter()
            .collect(),
    }
}

/// With `--static`: builds the app (`services.staticHosting.buildCommand`) for the backend to
/// serve.
pub async fn static_site(ctx: &ServeContext<'_>) -> anyhow::Result<Option<StaticSite>> {
    if !ctx.options.static_build {
        return Ok(None);
    }
    let hosting = &ctx.project.config().services.static_hosting;
    let index_document = hosting.index_document().to_owned();
    let root = rl_frontend::build_static(StaticBuildConfig {
        frontend_dir: ctx.project.static_build_dir().to_path_buf(),
        build_command: hosting.build_command.clone(),
        output_dir: ctx.project.static_output_dir().to_path_buf(),
        index_document: Some(index_document.clone()),
        env: rl_frontend::frontend_env(&rayfin_env(ctx)),
        sink: ui::child_sink(),
    })
    .await
    .context("Could not build the app for --static")?;
    ui::step(format_args!("Built the app into {}", root.display()));
    Ok(Some(StaticSite {
        root,
        index_document,
    }))
}

/// Starts the app's Vite dev server; returns its URL. When Vite exits on its own, the backend
/// keeps running.
pub async fn start_frontend(
    ctx: &ServeContext<'_>,
    hooks: &mut StopHooks,
) -> anyhow::Result<Option<String>> {
    let Some(port) = ctx.frontend_port else {
        return Ok(None);
    };
    let server = DevServer::start(DevServerConfig {
        node: ctx.node.clone(),
        frontend_dir: ctx.project.frontend_dir().to_path_buf(),
        cache_dir: ctx.state.cache_dir(),
        host: dev_server_host(ctx.host).to_owned(),
        port,
        backend_origin: ctx.api_origin.to_owned(),
        env: rl_frontend::frontend_env(&rayfin_env(ctx)),
        sink: ui::child_sink(),
    })
    .await
    .context("Could not start the app's Vite dev server")?;
    ui::step(format_args!(
        "Vite {} dev server on {}",
        server.vite_version(),
        server.url()
    ));
    let url = server.url().to_owned();
    let (stop, stopped) = oneshot::channel();
    let supervisor = tokio::spawn(supervise_dev_server(
        server,
        stopped,
        ctx.api_origin.to_owned(),
    ));
    hooks.add("the Vite dev server", move || async move {
        let _ = stop.send(());
        let _ = supervisor.await;
    });
    Ok(Some(url))
}

/// Stops the dev server when `stop` fires, or reports that it exited by itself.
async fn supervise_dev_server(
    mut server: DevServer,
    stop: oneshot::Receiver<()>,
    api_origin: String,
) {
    tokio::select! {
        exit = server.wait_exit() => {
            let how = match exit {
                Ok(status) => status.to_string(),
                Err(err) => err.to_string(),
            };
            ui::warn(format_args!(
                "The Vite dev server stopped ({how}). The backend keeps running on {api_origin}; \
                 to start the app again, stop rayfin-local with Ctrl+C and run \
                 `rayfin-local serve` again."
            ));
        }
        _ = stop => {
            if let Err(err) = server.stop().await {
                tracing::warn!("could not stop the Vite dev server: {err}");
            }
        }
    }
}

/// The interface Vite listens on: `localhost` when the backend is on loopback (the backend then
/// answers on both loopback addresses, and apps list `http://localhost:<port>` in
/// `allowedRedirectUris`), else the backend's.
fn dev_server_host(host: &str) -> &str {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.eq_ignore_ascii_case("localhost") || matches!(bare, "127.0.0.1" | "::1") {
        "localhost"
    } else {
        host
    }
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

    #[test]
    fn the_storage_signing_key_is_kept_and_a_bad_one_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys").join(STORAGE_KEY_FILE);
        let key = signing_key(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), hex::encode(key));
        assert_eq!(signing_key(&path).unwrap(), key);

        std::fs::write(&path, "not hex").unwrap();
        let replaced = signing_key(&path).unwrap();
        assert_ne!(replaced, key);
        assert_eq!(signing_key(&path).unwrap(), replaced);
    }

    #[test]
    fn secrets_come_from_the_environment_then_the_local_store() {
        let stored = BTreeMap::from([
            ("BOTH".to_owned(), "stored".to_owned()),
            ("STORED".to_owned(), "stored".to_owned()),
        ]);
        let settings = BTreeSet::from(["IN_SETTINGS".to_owned()]);
        let env = |name: &str| match name {
            "BOTH" | "ENV" => Some("env".to_owned()),
            "EMPTY" => Some(String::new()),
            _ => None,
        };
        let names = ["BOTH", "ENV", "STORED", "IN_SETTINGS", "EMPTY", "BOTH", " "];
        let (values, missing) = resolve_secrets(names, env, &stored, &settings);
        let values: Vec<_> = values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        assert_eq!(
            values,
            [("BOTH", "env"), ("ENV", "env"), ("STORED", "stored")]
        );
        assert_eq!(missing, ["EMPTY"]);
    }
}
