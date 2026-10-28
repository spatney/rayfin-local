//! `rayfin-local serve`: every backend service of the project on this machine, and its frontend.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use axum::Router;
use rl_auth::{AuthService, AuthSettings};
use rl_db::Db;
use rl_model::AppModel;
use rl_project::{LocalState, Project};
use rl_server::{ServerConfig, StaticSite};
use rl_tokens::{TokenBroker, TokenMode, TokenSource};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::analysis;
use crate::integrations::{self, ServeContext};
use crate::ports::{self, DEFAULT_API_PORT, PortRequest};
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
    let mut state = open_state(&project, options.reset)?;
    let node = super::find_node().await?;
    let model = Arc::new(analysis::analyze(&project, &state, &node, options.verbose).await?);
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

    let ctx = ServeContext {
        project: &project,
        state: &state,
        node: &node,
        model: &model,
        db: &db,
        auth: &auth,
        broker: &broker,
        options: &options,
        host: &host,
        api_origin: &api_origin,
        frontend_port: bound.frontend_port,
    };
    let mut running = Running::new();
    let outcome = serve_until_signal(&ctx, bound.listeners, &auth_settings, &mut running).await;
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

/// `--reset`, then the project's `.rayfin-local/` (created on first use).
fn open_state(project: &Project, reset: bool) -> anyhow::Result<LocalState> {
    if reset && LocalState::reset(project)? {
        ui::step(format_args!(
            "Deleted {} (--reset)",
            project.state_dir().display()
        ));
    }
    Ok(LocalState::load_or_create(project)?)
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
        ui::warn(format_args!(
            "The backend listens on {host}, so other machines can reach it. It acts with your \
             Azure identity for connectors and functions; use 127.0.0.1 unless you need this."
        ));
    }
    host
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
    let port = ports::choose_port(request, None, is_free)?;
    let listeners = rl_server::bind(host, port).await?;
    // Port 0 asks the system for one.
    let api_port = listeners
        .first()
        .and_then(|listener| listener.local_addr().ok())
        .map_or(port, |addr| addr.port());

    let frontend_port = if options.no_frontend || options.static_build {
        None
    } else {
        let request = PortRequest::resolve(
            "--frontend-port",
            options.frontend_port,
            "[frontend] port",
            local.frontend.port,
            project.preferred_frontend_port(),
        );
        Some(ports::choose_port(request, Some(api_port), is_free)?)
    };

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
    running: &mut Running,
) -> anyhow::Result<()> {
    let features = feature_routers(ctx, &mut running.hooks).await?;
    let static_site = integrations::static_site(ctx).await?;
    let serves_static = static_site.is_some();
    let router = rl_server::build_router(server_config(ctx, static_site), features);
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
    let frontend = if ctx.options.no_frontend {
        "not started (--no-frontend)".to_owned()
    } else if serves_static {
        format!("{} (static build)", ctx.api_origin)
    } else if let Some(url) = &frontend_url {
        url.clone()
    } else if ctx.options.static_build {
        "not built (static hosting is not wired yet)".to_owned()
    } else {
        "not started (the dev server is not wired yet)".to_owned()
    };
    ui::banner(&ui::Banner {
        app_name: ctx.project.display_name(),
        api_url: ctx.api_origin,
        frontend,
        data_dir: ctx.state.dir(),
        sign_in: sign_in_hint(auth_settings),
    });
    if ctx.options.open {
        let url = frontend_url.as_deref().unwrap_or(ctx.api_origin);
        if let Err(err) = open::that_detached(url) {
            ui::warn(format_args!(
                "Could not open a browser ({err}); open {url} yourself."
            ));
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

/// The feature routers: auth and connectors, then the services wired in `integrations`.
async fn feature_routers(
    ctx: &ServeContext<'_>,
    hooks: &mut StopHooks,
) -> anyhow::Result<Vec<Router>> {
    let mut routers = vec![
        Arc::clone(ctx.auth).router(),
        integrations::connectors_router(ctx),
    ];
    routers.extend(integrations::graphql_router(ctx)?);
    routers.extend(integrations::storage_router(ctx)?);
    routers.extend(integrations::functions_router(ctx, hooks).await?);
    Ok(routers)
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
    let mut ways = Vec::new();
    if settings.password_enabled && settings.signup_enabled {
        ways.push("sign up in the app with any email and password (users stay on this machine)");
    }
    if settings.fabric_enabled {
        ways.push("\"Sign in with Fabric\" opens the local sign-in page");
    }
    if ways.is_empty() {
        "no sign-in method is enabled in services.auth of rayfin.yml".to_owned()
    } else {
        ways.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
