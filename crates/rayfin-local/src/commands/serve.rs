//! `rayfin-local serve`: every backend service of the project on this machine, and its frontend.

use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use axum::Router;
use rl_auth::{AuthService, AuthSettings};
use rl_db::Db;
use rl_functions::FunctionsService;
use rl_model::AppModel;
use rl_project::{LocalState, Project, ServeInfo, ServeLock};
use rl_server::{ServerConfig, StaticSite};
use rl_tokens::{TokenBroker, TokenMode, TokenSource};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::analysis;
use crate::integrations::{self, ServeContext};
use crate::ports::{self, DEFAULT_API_PORT, PortRequest};
use crate::reload;
use crate::shutdown::{self, StopHooks};
use crate::ui;

/// The interface to listen on when neither `--host` nor `[server] host` is set.
const DEFAULT_HOST: &str = "127.0.0.1";
/// How long the backend has to answer its health check once it listens.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long each started process has to stop.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// How long the backend has to finish open requests (it allows them 5 seconds itself).
const SERVER_STOP_TIMEOUT: Duration = Duration::from_secs(7);
/// The exit code after Ctrl+C (128 + SIGINT, as shells report it).
const INTERRUPTED: u8 = 130;

/// `serve`'s command-line settings.
#[derive(Clone, Debug, Default)]
pub struct ServeOptions {
    pub port: Option<u16>,
    pub frontend_port: Option<u16>,
    pub static_build: bool,
    pub no_frontend: bool,
    pub no_functions: bool,
    pub host: Option<String>,
    pub token_source: Option<TokenMode>,
    pub tenant: Option<String>,
    pub force_schema: bool,
    pub reset: bool,
    pub inspect: Option<u16>,
    pub open: bool,
    pub verbose: bool,
}

pub async fn run(dir: &Path, options: ServeOptions) -> anyhow::Result<ExitCode> {
    let project = super::load_project(dir)?;
    let (mut state, serve_lock) = open_state(&project, options.reset)?;
    let node = super::find_node().await?;
    let (model, model_stamp) = analysis::analyze(&project, &state, &node, options.verbose).await?;
    check_anonymous_access(&project, &model)?;
    let model = Arc::new(model);
    let db = open_database(&project, &state, &model, options.force_schema).await?;
    let broker = Arc::new(integrations::token_broker(
        &project,
        &state,
        Some(&node),
        options.token_source,
        options.tenant.clone(),
    )?);
    let host = listen_host(&project, &options);
    let (auth, auth_settings) = start_auth(&project, &state, &db, &host, &broker).await?;
    let bound = bind_ports(&project, &mut state, &options, &host).await?;
    let api_origin = format!("http://{}:{}", origin_host(&host), bound.api_port);
    let data_api = integrations::data_api(&project, &db, &model, auth.clone());

    let ctx = ServeContext {
        project: &project,
        state: &state,
        node: &node,
        model: &model,
        model_stamp: &model_stamp,
        db: &db,
        auth: &auth,
        broker: &broker,
        data_api,
        options: &options,
        host: &host,
        api_origin: &api_origin,
        frontend_port: bound.frontend_port,
    };
    let mut running = Running::new();
    let outcome = serve_until_signal(
        &ctx,
        bound.listeners,
        &auth_settings,
        &serve_lock,
        &mut running,
    )
    .await;
    if outcome.is_ok() {
        ui::status("Stopping…");
        tokio::spawn(async {
            if shutdown::signal().await.is_ok() {
                eprintln!();
                std::process::exit(i32::from(INTERRUPTED));
            }
        });
    }
    running.shut_down().await;
    outcome?;
    ui::status("Stopped.");
    Ok(ExitCode::from(INTERRUPTED))
}

/// The project's `.rayfin-local/` (created on first use), locked against a second `serve`.
/// `--reset` then empties the database and storage, which hold the users, the entity data and
/// the stored files.
fn open_state(project: &Project, reset: bool) -> anyhow::Result<(LocalState, ServeLock)> {
    let state = LocalState::load_or_create(project)?;
    let lock = state.lock_serve()?;
    if reset {
        let cleared = clear_data(&state)?;
        let what = if cleared.is_empty() {
            "nothing to delete".to_owned()
        } else {
            format!("deleted {}", cleared.join(", "))
        };
        ui::step(format_args!(
            "Started from an empty database (--reset: {what} in {}; kept the ports, keys, \
             secrets and caches)",
            state.dir().display()
        ));
    }
    Ok((state, lock))
}

/// Deletes the database (with its SQLite side files) and the stored files; returns the names of
/// those that existed.
fn clear_data(state: &LocalState) -> anyhow::Result<Vec<String>> {
    let in_use = "Stop any other rayfin-local serving this project and close programs that have \
                  its database or files open, then try again.";
    let mut cleared = Vec::new();
    let db = state.db_path();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut path = db.clone().into_os_string();
        path.push(suffix);
        let path = PathBuf::from(path);
        match fs::remove_file(&path) {
            Ok(()) => cleared.push(file_name(&path)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Could not delete {}. {in_use}", path.display()));
            }
        }
    }
    let storage = state.storage_dir();
    let had_files = fs::read_dir(&storage).is_ok_and(|mut entries| entries.next().is_some());
    match fs::remove_dir_all(&storage) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("Could not delete {}. {in_use}", storage.display()));
        }
    }
    if had_files {
        cleared.push(format!("{}/", file_name(&storage)));
    }
    fs::create_dir_all(&storage)
        .with_context(|| format!("Could not create {}", storage.display()))?;
    Ok(cleared)
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The project database, with entity tables matching the model (when `services.data` is on).
async fn open_database(
    project: &Project,
    state: &LocalState,
    model: &AppModel,
    force: bool,
) -> anyhow::Result<Db> {
    let path = state.db_path();
    let db = Db::open(&path).with_context(|| {
        format!(
            "Could not open the database {}. Close any program that has it open, or start from \
             an empty one with `rayfin-local serve --reset`.",
            path.display()
        )
    })?;
    // With the data service off, the existing tables are left alone rather than dropped.
    if project.config().services.data.enabled {
        integrations::apply_schema(&db, model, force).await?;
    }
    Ok(db)
}

/// `--host`, else `[server] host`, else loopback.
fn listen_host(project: &Project, options: &ServeOptions) -> String {
    let host = options
        .host
        .clone()
        .or_else(|| project.local_config().server.host.clone())
        .map(|host| host.trim().to_owned())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| DEFAULT_HOST.to_owned());
    if !integrations::is_loopback(&host) {
        let mut message = format!(
            "The backend listens on {host}, so other machines can reach it. It acts with your \
             Azure identity for connectors and functions; use 127.0.0.1 unless you need this."
        );
        if is_wildcard(&host) {
            message.push_str(
                " The app's URLs point at localhost, which other devices can't reach: to use the \
                 app from another device, use `--host <this machine's LAN address>` instead.",
            );
        }
        ui::warn(message);
    }
    host
}

/// `0.0.0.0` or `::`: every interface.
fn is_wildcard(host: &str) -> bool {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok_and(|ip| ip.is_unspecified())
}

/// The auth service. Signing in as the developer's Entra identity ("Continue as <you>" and the
/// Entra exchange) is on only on loopback, with the developer's tokens from `broker`.
async fn start_auth(
    project: &Project,
    state: &LocalState,
    db: &Db,
    host: &str,
    broker: &Arc<TokenBroker>,
) -> anyhow::Result<(Arc<AuthService>, AuthSettings)> {
    let settings = integrations::auth_settings(
        project,
        &state.data().app_id,
        integrations::is_loopback(host),
    );
    let auth = AuthService::new(db.clone(), state.keys_dir(), settings.clone())
        .await
        .context("Could not start the local auth service")?
        .with_entra(Arc::clone(broker) as Arc<dyn TokenSource>);
    Ok((Arc::new(auth), settings))
}

struct BoundPorts {
    listeners: Vec<TcpListener>,
    api_port: u16,
    /// `None` when no dev server runs (`--no-frontend`, `--static`).
    frontend_port: Option<u16>,
}

/// Chooses the ports, listens on the backend's, and remembers both in `state.json`.
async fn bind_ports(
    project: &Project,
    state: &mut LocalState,
    options: &ServeOptions,
    host: &str,
) -> anyhow::Result<BoundPorts> {
    let local = project.local_config();
    let is_free = |port| rl_server::is_port_free(host, port);
    let request = PortRequest::resolve(
        "--port",
        options.port,
        "[server] port",
        local.server.port,
        state.data().api_port.unwrap_or(DEFAULT_API_PORT),
    );
    let frontend_request = (!options.no_frontend && !options.static_build).then(|| {
        PortRequest::resolve(
            "--frontend-port",
            options.frontend_port,
            "[frontend] port",
            local.frontend.port,
            project.preferred_frontend_port(),
        )
    });
    let port = ports::choose_port(
        request,
        request.yields_to(frontend_request.as_ref()),
        is_free,
    )?;
    let listeners = rl_server::bind(host, port).await?;
    // Port 0 asks the system for one.
    let api_port = listeners
        .first()
        .and_then(|listener| listener.local_addr().ok())
        .map_or(port, |addr| addr.port());

    let frontend_port = frontend_request
        .map(|request| ports::choose_port(request, Some(api_port), is_free))
        .transpose()?;

    let data = state.data_mut();
    data.api_port = Some(api_port);
    if frontend_port.is_some() {
        data.frontend_port = frontend_port;
    }
    state.save()?;
    Ok(BoundPorts {
        listeners,
        api_port,
        frontend_port,
    })
}

/// The host name for URLs: `localhost` for loopback and wildcard addresses (the backend listens
/// on both 127.0.0.1 and ::1 for those), else the host itself.
fn origin_host(host: &str) -> String {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(ip)
            if ip.is_unspecified()
                || ip == IpAddr::V4(Ipv4Addr::LOCALHOST)
                || ip == IpAddr::V6(Ipv6Addr::LOCALHOST) =>
        {
            "localhost".to_owned()
        }
        Ok(IpAddr::V6(ip)) => format!("[{ip}]"),
        Ok(ip) => ip.to_string(),
        Err(_) => bare.to_ascii_lowercase(),
    }
}

/// The Host header check: off for wildcard addresses (other machines use names we can't know),
/// else the host (loopback names are always allowed).
fn allowed_hosts(host: &str) -> Option<Vec<String>> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<IpAddr>() {
        Ok(ip) if ip.is_unspecified() => None,
        _ => Some(vec![bare.to_owned()]),
    }
}

/// What `serve` started, to stop on the way out.
struct Running {
    stop: watch::Sender<bool>,
    server: Option<JoinHandle<io::Result<()>>>,
    hooks: StopHooks,
}

impl Running {
    fn new() -> Self {
        Self {
            stop: watch::channel(false).0,
            server: None,
            hooks: StopHooks::default(),
        }
    }

    /// Stops the backend (letting open requests finish) and every started process.
    async fn shut_down(self) {
        let _ = self.stop.send(true);
        self.hooks.run(STOP_GRACE).await;
        if let Some(server) = self.server {
            match tokio::time::timeout(SERVER_STOP_TIMEOUT, server).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(err))) => tracing::warn!("the backend stopped with an error: {err}"),
                Ok(Err(err)) => tracing::warn!("the backend task failed: {err}"),
                Err(_) => tracing::warn!("the backend did not stop in time"),
            }
        }
    }
}

/// Starts the backend and the frontend, shows the banner, and waits for Ctrl+C. Returns an
/// error if anything fails to start or the backend stops on its own.
async fn serve_until_signal(
    ctx: &ServeContext<'_>,
    listeners: Vec<TcpListener>,
    auth_settings: &AuthSettings,
    serve_lock: &ServeLock,
    running: &mut Running,
) -> anyhow::Result<()> {
    let static_site = integrations::static_site(ctx).await?;
    let serves_static = static_site.is_some();
    let (routers, features) = start_features(ctx, &mut running.hooks).await?;
    let router = rl_server::build_router(server_config(ctx, static_site), routers);
    let mut stop = running.stop.subscribe();
    let server = running.server.insert(tokio::spawn(rl_server::serve(
        router,
        listeners,
        async move {
            // An error means `Running` is gone, which also means stop.
            let _ = stop.wait_for(|stop| *stop).await;
        },
    )));
    tokio::select! {
        ready = wait_until_ready(ctx.api_origin) => ready?,
        result = &mut *server => return Err(server_stopped(result)),
    }
    ui::step(format_args!("Backend listening on {}", ctx.api_origin));

    let frontend_url = integrations::start_frontend(ctx, &mut running.hooks).await?;
    if let Err(err) = reload::watch(ctx, &mut running.hooks) {
        ui::warn(format_args!(
            "Live reload is off ({err:#}); restart rayfin-local after changing the entities."
        ));
    }
    let app_url = if serves_static {
        Some(ctx.api_origin.to_owned())
    } else {
        frontend_url
    };
    // What the local Fabric host frames, for apps that sign in embedded.
    ctx.auth.set_embed_app_url(app_url.clone());
    ui::banner(&ui::Banner {
        app_name: ctx.project.display_name(),
        rows: banner_rows(ctx, &features, app_url.as_deref(), auth_settings),
    });
    // For the error a second `serve` on this project reports.
    let published = serve_lock.publish(&ServeInfo {
        pid: std::process::id(),
        app_url: app_url.clone(),
        api_url: Some(ctx.api_origin.to_owned()),
    });
    if let Err(err) = published {
        tracing::debug!("{err}");
    }
    if ctx.options.open {
        match &app_url {
            Some(url) => {
                if let Err(err) = open::that_detached(url) {
                    ui::warn(format_args!(
                        "Could not open a browser ({err}); open {url} yourself."
                    ));
                }
            }
            None => ui::warn("--open: there is no app to open with --no-frontend."),
        }
    }

    let server = running
        .server
        .as_mut()
        .expect("the backend was started above");
    tokio::select! {
        signal = shutdown::signal() => {
            signal.context("Could not listen for Ctrl+C")?;
            Ok(())
        }
        result = server => {
            running.server = None;
            Err(server_stopped(result))
        }
    }
}

fn server_stopped(result: Result<io::Result<()>, tokio::task::JoinError>) -> anyhow::Error {
    match result {
        Ok(Ok(())) => anyhow!("The backend stopped unexpectedly."),
        Ok(Err(err)) => anyhow!(err).context("The backend stopped"),
        Err(err) => anyhow!("The backend crashed: {err}"),
    }
}

/// The feature services that started, for the banner.
struct Features {
    functions: Option<Arc<FunctionsService>>,
    storage: bool,
}

/// The feature routers (auth and connectors, then the services configured in `integrations`),
/// and what started. With `[fabric] anonymous_data_access = false`, the routes Fabric's
/// authentication middleware guards answer requests without a token with its 401.
async fn start_features(
    ctx: &ServeContext<'_>,
    hooks: &mut StopHooks,
) -> anyhow::Result<(Vec<Router>, Features)> {
    let reject_anonymous = ctx.project.local_config().fabric.anonymous_data_access == Some(false);
    let guarded = |router: Router| {
        if reject_anonymous {
            router.layer(axum::middleware::from_fn(rl_server::reject_anonymous))
        } else {
            router
        }
    };
    let mut routers = vec![
        Arc::clone(ctx.auth).router(),
        guarded(integrations::connectors_router(ctx).await),
    ];
    routers.extend(integrations::graphql_router(ctx).map(guarded));
    let storage = integrations::storage_router(ctx).await?;
    let has_storage = storage.is_some();
    routers.extend(storage.map(guarded));
    let functions = integrations::start_functions(ctx, hooks).await?;
    routers.extend(functions.as_ref().map(|service| guarded(service.router())));
    let features = Features {
        functions,
        storage: has_storage,
    };
    Ok((routers, features))
}

/// The banner's rows: where the app and the API are, what runs, and how to sign in.
fn banner_rows(
    ctx: &ServeContext<'_>,
    features: &Features,
    app_url: Option<&str>,
    auth_settings: &AuthSettings,
) -> Vec<(&'static str, String)> {
    let config = ctx.project.config();
    let app = match app_url {
        Some(url) if ctx.options.static_build => format!("{url} (static build)"),
        Some(url) => url.to_owned(),
        None => "not started (--no-frontend)".to_owned(),
    };
    let entities = if config.services.data.enabled {
        count(ctx.model.entities.len(), "entity", "entities")
    } else {
        "off (services.data is disabled in rayfin.yml)".to_owned()
    };
    let functions = match &features.functions {
        Some(service) => match service.functions() {
            names if names.is_empty() => "none loaded (see the function output above)".to_owned(),
            names => format!(
                "{} ({})",
                count(names.len(), "function", "functions"),
                name_list(&names)
            ),
        },
        None if ctx.options.no_functions => "off (--no-functions)".to_owned(),
        None => "off (services.functions is disabled in rayfin.yml)".to_owned(),
    };
    let connectors = integrations::connectors_summary(&config.connectors);
    let folders = integrations::storage_folders(ctx.model);
    let storage = if features.storage {
        format!(
            "{} ({})",
            count(folders.len(), "folder", "folders"),
            folders.join(", ")
        )
    } else if config.services.storage.enabled {
        "off (no folders in rayfin/storage)".to_owned()
    } else {
        "off (services.storage is disabled in rayfin.yml)".to_owned()
    };
    let mut rows = vec![("App", app)];
    if app_url.is_some() && auth_settings.enabled && auth_settings.fabric_enabled {
        rows.push((
            "Embedded",
            format!(
                "{}{} (the app inside a local Fabric frame)",
                ctx.api_origin,
                rl_auth::EMBED_HOST_PATH
            ),
        ));
    }
    rows.extend([
        ("API", ctx.api_origin.to_owned()),
        ("Entities", entities),
        ("Functions", functions),
        ("Connectors", connectors),
        ("Storage", storage),
        ("Data", ctx.state.dir().display().to_string()),
        ("Sign in", sign_in_hint(auth_settings)),
    ]);
    rows
}

/// The entities that grant the `anonymous` role.
fn anonymous_entities(model: &AppModel) -> Vec<&str> {
    model
        .entities
        .iter()
        .filter(|entity| {
            entity.permissions.iter().any(|permission| {
                permission.role.eq_ignore_ascii_case("anonymous") && !permission.actions.is_empty()
            })
        })
        .map(|entity| entity.name.as_str())
        .collect()
}

/// Fabric accepts the `anonymous` role only when a tenant administrator allows anonymous data
/// access, which is off by default. `[fabric] anonymous_data_access = false` refuses such
/// entities, as `rayfin up` does on such a tenant; leaving it unset warns about them.
fn check_anonymous_access(project: &Project, model: &AppModel) -> anyhow::Result<()> {
    let entities = anonymous_entities(model);
    let (subject, verb) = match entities.as_slice() {
        [] => return Ok(()),
        [one] => (format!("Entity {one}"), "grants"),
        many => (format!("Entities {}", many.join(", ")), "grant"),
    };
    let setting = "\"Enable anonymous data access for Fabric Apps (Preview)\"";
    match project.local_config().fabric.anonymous_data_access {
        Some(true) => Ok(()),
        Some(false) => bail!(
            "{subject} {verb} the anonymous role, which a Fabric tenant without {setting} refuses: \
             `rayfin up` fails with ANONYMOUS_DATA_ACCESS_BLOCKED_BY_TENANT_ADMIN, and \
             rayfin-local.toml asks rayfin-local to act like such a tenant ([fabric] \
             anonymous_data_access = false). Grant access with @authenticated instead, or change \
             the setting."
        ),
        None => {
            ui::warn(format_args!(
                "{subject} {verb} the anonymous role. Fabric accepts it only when a tenant \
                 administrator turns on {setting}, which is off by default; otherwise `rayfin up` \
                 refuses the schema. In rayfin-local.toml, set [fabric] anonymous_data_access to \
                 true if your tenant allows it (hides this warning), or to false to act like a \
                 tenant that doesn't."
            ));
            Ok(())
        }
    }
}

/// `1 entity`, `3 entities`.
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// How many names a banner row shows.
const BANNER_NAMES: usize = 8;

/// `names` joined, cut to the first [`BANNER_NAMES`] and `… and N more`.
fn name_list(names: &[String]) -> String {
    match names.get(BANNER_NAMES..) {
        Some(rest) if !rest.is_empty() => format!(
            "{} … and {} more",
            names[..BANNER_NAMES].join(", "),
            rest.len()
        ),
        _ => names.join(", "),
    }
}

fn server_config(ctx: &ServeContext<'_>, static_site: Option<StaticSite>) -> ServerConfig {
    let data = ctx.state.data();
    ServerConfig {
        api_origin: ctx.api_origin.to_owned(),
        publishable_key: data.publishable_key.clone(),
        workspace_id: data.workspace_id.clone(),
        item_id: data.item_id.clone(),
        tenant_id: data.tenant_id.clone(),
        portal_url: format!("{}{}", ctx.api_origin, rl_auth::PORTAL_PATH),
        allowed_origins: ctx
            .project
            .config()
            .services
            .auth
            .allowed_redirect_uris
            .clone(),
        allowed_hosts: allowed_hosts(ctx.host),
        static_site,
        publishable_key_exempt: rl_auth::PUBLISHABLE_KEY_EXEMPT_PATHS
            .iter()
            .map(|path| (*path).to_owned())
            .collect(),
    }
}

/// Polls the health route until the backend answers.
async fn wait_until_ready(api_origin: &str) -> anyhow::Result<()> {
    let url = format!("{api_origin}{}", rl_server::HEALTH_PATH);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()
        .context("Could not create an HTTP client")?;
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let problem = match client.get(&url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => format!("it answered {}", response.status()),
            Err(err) => err.to_string(),
        };
        if Instant::now() >= deadline {
            bail!(
                "The backend did not answer {url} within {} seconds ({problem}). Check that no \
                 firewall or proxy blocks local connections, then try again.",
                READY_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn sign_in_hint(settings: &AuthSettings) -> String {
    if !settings.enabled {
        return "auth is off (services.auth.enabled is false in rayfin.yml)".to_owned();
    }
    if settings.fabric_enabled {
        let mut hint = "Fabric sign-in opens a local page: create any account there".to_owned();
        if settings.entra_signin.enabled {
            hint.push_str(", or continue as your Azure identity");
        }
        return hint;
    }
    if settings.password_enabled && settings.signup_enabled {
        return "sign up in the app with any email and password (users stay on this machine)"
            .to_owned();
    }
    "no sign-in method is enabled in services.auth of rayfin.yml".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_clears_the_database_and_files_but_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("rayfin")).unwrap();
        fs::write(dir.path().join("rayfin").join("rayfin.yml"), "name: app\n").unwrap();
        let project = Project::load(dir.path()).unwrap();
        let state = LocalState::load_or_create(&project).unwrap();
        let db = state.db_path();
        fs::write(&db, "db").unwrap();
        fs::write(db.with_extension("sqlite-wal"), "wal").unwrap();
        fs::create_dir_all(state.storage_dir().join("objects")).unwrap();
        fs::write(state.storage_dir().join("objects").join("a"), "a").unwrap();
        fs::write(state.keys_dir().join("key.pem"), "key").unwrap();

        let cleared = clear_data(&state).unwrap();
        assert_eq!(cleared, ["db.sqlite", "db.sqlite-wal", "storage/"]);
        assert!(!db.exists());
        assert!(state.storage_dir().is_dir());
        assert_eq!(fs::read_dir(state.storage_dir()).unwrap().count(), 0);
        assert!(state.keys_dir().join("key.pem").is_file());
        assert!(state.dir().join("state.json").is_file());
        assert!(clear_data(&state).unwrap().is_empty());
    }

    #[test]
    fn urls_use_localhost_for_loopback_and_wildcard_hosts() {
        for host in ["127.0.0.1", "::1", "[::1]", "localhost", "0.0.0.0", "::"] {
            assert_eq!(origin_host(host), "localhost", "{host}");
        }
        assert_eq!(origin_host("192.168.1.10"), "192.168.1.10");
        assert_eq!(origin_host("fe80::1"), "[fe80::1]");
        assert_eq!(allowed_hosts("0.0.0.0"), None);
        assert_eq!(allowed_hosts("[::1]"), Some(vec!["::1".to_owned()]));
    }

    #[test]
    fn the_banner_lists_at_most_eight_names() {
        let names: Vec<String> = (1..=10).map(|n| format!("f{n}")).collect();
        assert_eq!(
            name_list(&names),
            "f1, f2, f3, f4, f5, f6, f7, f8 … and 2 more"
        );
        assert_eq!(name_list(&names[..8]), "f1, f2, f3, f4, f5, f6, f7, f8");
    }

    #[test]
    fn anonymous_grants_follow_the_tenant_setting() {
        let model =
            |json: &str| AppModel::from_analysis(&serde_json::from_str(json).unwrap()).unwrap();
        let basic = model(include_str!(
            "../../../rl-model/tests/fixtures/basic.analysis.json"
        ));
        let mut names = anonymous_entities(&basic);
        names.sort_unstable();
        assert_eq!(names, ["Category", "Comment", "Project", "Todo"]);
        let explorer = model(include_str!(
            "../../../rl-model/tests/fixtures/explorer.analysis.json"
        ));
        assert!(anonymous_entities(&explorer).is_empty());

        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("rayfin")).unwrap();
        fs::write(dir.path().join("rayfin").join("rayfin.yml"), "name: app\n").unwrap();
        let project = |setting: &str| {
            fs::write(dir.path().join("rayfin-local.toml"), setting).unwrap();
            Project::load(dir.path()).unwrap()
        };
        let refused = check_anonymous_access(
            &project("[fabric]\nanonymous_data_access = false\n"),
            &basic,
        )
        .unwrap_err()
        .to_string();
        assert!(
            refused.starts_with("Entities ") && refused.contains("anonymous_data_access = false"),
            "{refused}"
        );
        assert!(
            check_anonymous_access(
                &project("[fabric]\nanonymous_data_access = false\n"),
                &explorer
            )
            .is_ok()
        );
        assert!(
            check_anonymous_access(&project("[fabric]\nanonymous_data_access = true\n"), &basic)
                .is_ok()
        );
        assert!(check_anonymous_access(&project(""), &basic).is_ok());
    }
}
