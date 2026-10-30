//! `rayfin-local serve`: every backend service of the project on this machine, and its frontend.
//!
//! With `--json` it is run by a host (docs/embedding.md): it reports what happens as events, and
//! stops when stdin ends as on Ctrl+C.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use axum::Router;
use ring::rand::{SecureRandom as _, SystemRandom};
use rl_auth::{AuthService, AuthSettings, SignInAs as Identity};
use rl_connectors::SqlConnectorModel;
use rl_db::Db;
use rl_model::AppModel;
use rl_project::{LocalState, Project, ServeInfo, ServeLock};
use rl_server::{RouterHandle, ServerConfig, StaticSite};
use rl_tokens::{TokenBroker, TokenMode};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::analysis;
use crate::control;
use crate::errors::{HostError, WithCode, coded};
use crate::events::{
    self, Analysis, AuthFeature, ConnectorFeature, DataFeature, ErrorCode, Event, FunctionsFeature,
    ProjectInfo, Ready, Schema, Sdk, SignInAs, Starting, StopReason, Stopped, StorageFeature,
    WarningCode,
};
use crate::integrations::{self, FunctionsSlot, SecretValues, ServeContext};
use crate::ports::{self, DEFAULT_API_PORT, PortRequest};
use crate::reload;
use crate::shutdown::{self, StopHooks, StopListener, StopRequest};
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
/// The variable that turns on the control API for hosts.
const CONTROL_TOKEN_VAR: &str = "RAYFIN_LOCAL_CONTROL_TOKEN";
/// The shortest control token accepted.
const MIN_CONTROL_TOKEN_CHARS: usize = 32;

/// `serve`'s command-line settings.
#[derive(Clone, Debug, Default)]
pub struct ServeOptions {
    /// Events on stdout, and a graceful stop when stdin ends.
    pub json: bool,
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
    /// `--sign-in-as`: who apps are signed in as automatically.
    pub sign_in_as: Option<rl_project::SignInAs>,
    pub open: bool,
    pub verbose: bool,
}

/// How serving ended.
enum Outcome {
    /// Something asked it to stop.
    Stop(StopRequest),
    /// Starting failed.
    Failed(anyhow::Error),
    /// The backend stopped by itself.
    BackendExited(anyhow::Error),
}

pub async fn run(dir: &Path, options: ServeOptions) -> anyhow::Result<ExitCode> {
    let mut running = Running::new();
    let outcome = if options.json {
        // From the start: a host may give up on a start that takes long.
        let mut stop = StopListener::with_input(io::stdin());
        tokio::select! {
            outcome = serve(dir, &options, &mut running) => outcome,
            request = stop.requested() => match request {
                Ok(request) => Outcome::Stop(request),
                Err(err) => Outcome::Failed(anyhow!(err).context("Could not listen for Ctrl+C")),
            },
        }
    } else {
        serve(dir, &options, &mut running).await
    };
    finish(outcome, running, options.json).await
}

/// Stops what runs, and reports how serving ended: with `--json` as events (the `error` first,
/// `stopped` last) and the exit code 0 after a stop request, else with the human output and 130
/// after Ctrl+C. Failures exit with 1.
async fn finish(outcome: Outcome, running: Running, json: bool) -> anyhow::Result<ExitCode> {
    match &outcome {
        Outcome::Stop(_) => {
            ui::status("Stopping…");
            tokio::spawn(async move {
                if shutdown::signal().await.is_ok() {
                    if json {
                        events::finish(Stopped {
                            reason: StopReason::Signal,
                            exit_code: INTERRUPTED,
                        });
                    } else {
                        eprintln!();
                    }
                    std::process::exit(i32::from(INTERRUPTED));
                }
            });
        }
        Outcome::Failed(err) | Outcome::BackendExited(err) if json => {
            events::emit(HostError::classify(err).event(true));
        }
        Outcome::Failed(_) | Outcome::BackendExited(_) => {}
    }
    running.shut_down().await;
    let (reason, code) = match outcome {
        Outcome::Stop(request) => {
            ui::status("Stopped.");
            let reason = match request {
                StopRequest::Signal => StopReason::Signal,
                StopRequest::StdinClosed => StopReason::StdinClosed,
            };
            (reason, if json { 0 } else { INTERRUPTED })
        }
        Outcome::Failed(err) | Outcome::BackendExited(err) if !json => return Err(err),
        Outcome::Failed(_) => (StopReason::Error, 1),
        Outcome::BackendExited(_) => (StopReason::BackendExited, 1),
    };
    if json {
        events::finish(Stopped {
            reason,
            exit_code: code,
        });
    }
    Ok(ExitCode::from(code))
}

/// Starts everything and serves until a stop request (without `--json`; with it, [`run`] listens
/// for them) or until the backend stops.
async fn serve(dir: &Path, options: &ServeOptions, running: &mut Running) -> Outcome {
    match start(dir, options, running).await {
        Ok(outcome) => outcome,
        Err(err) => Outcome::Failed(err),
    }
}

async fn start(
    dir: &Path,
    options: &ServeOptions,
    running: &mut Running,
) -> anyhow::Result<Outcome> {
    let project = Project::discover(dir)?;
    events::emit(Event::Starting(Starting {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        project: ProjectInfo {
            root: project.root().display().to_string(),
            name: project.display_name().to_owned(),
        },
    }));
    let control_token = check_control_token()?;
    super::check_project(&project)?;
    let (mut state, serve_lock) = open_state(&project, options.reset)?;
    running.lock = Some(serve_lock);
    let node = super::find_node().await?;
    let analyzed = analysis::analyze(&project, &state, &node, options.verbose).await;
    events::emit(Event::Analysis(Analysis::of(
        &project,
        analyzed.as_ref().map(|(model, _)| model),
    )));
    let (model, model_stamp) = analyzed.with_code(ErrorCode::AnalysisFailed)?;
    check_anonymous_access(&project, &model).with_code(ErrorCode::AnalysisFailed)?;
    let model = Arc::new(model);
    let db = open_database(&project, &state, &model, options.force_schema).await?;
    let broker = Arc::new(
        integrations::token_broker(
            &project,
            &state,
            Some(&node),
            options.token_source,
            options.tenant.clone(),
        )
        .with_code(ErrorCode::ConfigInvalid)?,
    );
    let host = listen_host(&project, options);
    let sign_in_secret = new_secret()?;
    let (auth, auth_settings) =
        start_auth(&project, &state, &db, &host, &broker, &sign_in_secret).await?;
    auth.set_sign_in_as(sign_in_identity(&project, options));
    let bound = bind_ports(&project, &mut state, options, &host).await?;
    let api_origin = format!("http://{}:{}", origin_host(&host), bound.api_port);
    let data_api = integrations::data_api(&project, &db, &model, auth.clone());
    // Live reload applies `rayfin.yml`, the entities, the connectors and the secret values on
    // demand through `reload` (the control API's `POST /reload`), once `reload_requests` is
    // served after the backend starts.
    let (reload, reload_requests) = reload::channel();

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
        options,
        host: &host,
        api_origin: &api_origin,
        frontend_port: bound.frontend_port,
        sign_in_secret: &sign_in_secret,
        control_token: control_token.as_deref(),
        reload,
    };
    serve_until_stopped(
        &ctx,
        bound.listeners,
        &auth_settings,
        running,
        reload_requests,
    )
    .await
}

/// `RAYFIN_LOCAL_CONTROL_TOKEN` as `serve` started ([`take_control_token`]).
static CONTROL_TOKEN: OnceLock<Option<OsString>> = OnceLock::new();

/// Takes `RAYFIN_LOCAL_CONTROL_TOKEN` out of the environment, so that no child process (Vite,
/// the function host, the app's code) inherits the host's secret. Call it first in `main`, while
/// no other thread runs.
pub fn take_control_token() {
    let token = std::env::var_os(CONTROL_TOKEN_VAR);
    if token.is_some() {
        // SAFETY: `main` calls this before it starts any thread.
        unsafe { std::env::remove_var(CONTROL_TOKEN_VAR) };
    }
    let _ = CONTROL_TOKEN.set(token);
}

/// `RAYFIN_LOCAL_CONTROL_TOKEN`, which turns on the control API: `None` when unset or empty, an
/// error when too short to be a secret.
fn check_control_token() -> anyhow::Result<Option<String>> {
    let token = CONTROL_TOKEN
        .get_or_init(|| std::env::var_os(CONTROL_TOKEN_VAR))
        .as_ref()
        .filter(|token| !token.is_empty());
    let Some(token) = token else {
        return Ok(None);
    };
    let token = token.to_string_lossy();
    let length = token.chars().count();
    if length < MIN_CONTROL_TOKEN_CHARS {
        return Err(coded(
            ErrorCode::ConfigInvalid,
            events::no_details(),
            anyhow!(
                "{CONTROL_TOKEN_VAR} is {length} characters long; the control API needs a secret \
                 of at least {MIN_CONTROL_TOKEN_CHARS}. Set it to a long random value, or unset \
                 it."
            ),
        ));
    }
    Ok(Some(token.into_owned()))
}

/// A new random secret for this run, as hex.
fn new_secret() -> anyhow::Result<String> {
    let mut bytes = [0; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow!("Could not generate a secret: the system has no randomness."))?;
    Ok(hex::encode(bytes))
}

/// `--sign-in-as`, else `[auth] sign_in_as`, else the developer.
pub(crate) fn sign_in_identity(project: &Project, options: &ServeOptions) -> Identity {
    let configured = options
        .sign_in_as
        .clone()
        .or_else(|| project.local_config().auth.sign_in_as.clone())
        .unwrap_or_default();
    match configured {
        rl_project::SignInAs::Developer => Identity::Developer,
        rl_project::SignInAs::User(username) => Identity::User(username),
        rl_project::SignInAs::None => Identity::None,
    }
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
    let db = Db::open(&path)
        .with_context(|| {
            format!(
                "Could not open the database {}. Close any program that has it open, or start \
                 from an empty one with `rayfin-local serve --reset`.",
                path.display()
            )
        })
        .inspect_err(|err| events::emit(Event::Schema(Schema::failed(err, false))))?;
    // With the data service off, the existing tables are left alone rather than dropped.
    if project.config().services.data.enabled {
        integrations::apply_schema(&db, model, force).await?;
    } else {
        events::emit(Event::Schema(Schema::unchanged(false)));
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

/// The auth service. Signing in as the developer's Entra identity ("Continue as <you>", the
/// Entra exchange and automatic sign-in) is on only on loopback, with the developer's tokens
/// from `broker`. They are optional there: without one, automatic sign-in signs in as the local
/// developer account and the sign-in page says why, so a missing token is no `azure-sign-in`
/// warning. Automatic sign-in answers the holder of `sign_in_secret` (the app's dev server).
async fn start_auth(
    project: &Project,
    state: &LocalState,
    db: &Db,
    host: &str,
    broker: &Arc<TokenBroker>,
    sign_in_secret: &str,
) -> anyhow::Result<(Arc<AuthService>, AuthSettings)> {
    let settings = integrations::auth_settings(
        project,
        &state.data().app_id,
        integrations::is_loopback(host),
    );
    let auth = AuthService::new(db.clone(), state.keys_dir(), settings.clone())
        .await
        .context("Could not start the local auth service")?
        .with_entra(broker.clone())
        .with_dev_sign_in_secret(sign_in_secret);
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
    let listeners = rl_server::bind(host, port).await.map_err(|err| {
        if matches!(
            err.kind(),
            io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
        ) {
            coded(ErrorCode::PortInUse, json!({ "port": port }), err)
        } else {
            anyhow::Error::new(err)
        }
    })?;
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
    /// Released last, once everything stopped.
    lock: Option<ServeLock>,
}

impl Running {
    fn new() -> Self {
        Self {
            stop: watch::channel(false).0,
            server: None,
            hooks: StopHooks::default(),
            lock: None,
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

/// Starts the backend and the frontend, shows the banner (`ready` with `--json`), starts live
/// reload (serving `reload_requests`), and serves until Ctrl+C (without `--json`) or until the
/// backend stops on its own. Returns an error if anything fails to start.
async fn serve_until_stopped(
    ctx: &ServeContext<'_>,
    listeners: Vec<TcpListener>,
    auth_settings: &AuthSettings,
    running: &mut Running,
    reload_requests: reload::Requests,
) -> anyhow::Result<Outcome> {
    let static_site = integrations::static_site(ctx).await?;
    let serves_static = static_site.is_some();
    let features = start_features(ctx).await?;
    let functions = features.functions.clone();
    running.hooks.add("the function host", move || async move {
        functions.stop().await;
    });
    // Live reload replaces the router; the listeners stay.
    let router = RouterHandle::new(rl_server::build_router(
        server_config(ctx, static_site.clone()),
        features.routers(ctx),
    ));
    let mut stop = running.stop.subscribe();
    let server = running.server.insert(tokio::spawn(rl_server::serve(
        router.router(),
        listeners,
        async move {
            // An error means `Running` is gone, which also means stop.
            let _ = stop.wait_for(|stop| *stop).await;
        },
    )));
    tokio::select! {
        ready = wait_until_ready(ctx.api_origin) => ready?,
        result = &mut *server => {
            running.server = None;
            return Err(server_stopped(result));
        }
    }
    ui::step(format_args!("Backend listening on {}", ctx.api_origin));

    let frontend_url = integrations::start_frontend(ctx, &mut running.hooks).await?;
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
    if let Some(lock) = &running.lock {
        let published = lock.publish(&ServeInfo {
            pid: std::process::id(),
            app_url: app_url.clone(),
            api_url: Some(ctx.api_origin.to_owned()),
        });
        if let Err(err) = published {
            tracing::debug!("{err}");
        }
    }
    let ready = ready_event(ctx, &features, app_url.clone(), auth_settings);
    // Its first changes come after `ready`: they wait for the edits to settle.
    reload::start(
        ctx,
        reload::Serving {
            features,
            router,
            static_site,
            app_url: app_url.clone(),
        },
        reload_requests,
        &mut running.hooks,
    );
    events::emit(Event::Ready(Box::new(ready)));
    if ctx.options.open && !ctx.options.json {
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
    // With `--json`, `run` listens for the stop requests from the start.
    let signal = async {
        if ctx.options.json {
            std::future::pending().await
        } else {
            shutdown::signal().await
        }
    };
    tokio::select! {
        signal = signal => {
            signal.context("Could not listen for Ctrl+C")?;
            Ok(Outcome::Stop(StopRequest::Signal))
        }
        result = server => {
            running.server = None;
            Ok(Outcome::BackendExited(server_stopped(result)))
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

/// The feature services that run, for the router, the banner and `ready`; live reload replaces
/// those whose settings change.
pub(crate) struct Features {
    /// `/connector-invoke/*` and `/connectors/*/graphql`.
    pub connectors: Router,
    /// The SQL connectors' entities `connectors` serves.
    pub connector_models: HashMap<String, SqlConnectorModel>,
    /// `/api/storage/*`, when `services.storage` is on and `rayfin/storage` declares folders.
    pub storage: Option<Router>,
    pub functions: FunctionsSlot,
    /// The secret values the functions run with.
    pub secrets: SecretValues,
}

impl Features {
    /// Every feature's router: auth and connectors, then the services configured in
    /// `integrations`, then the host's control API when `RAYFIN_LOCAL_CONTROL_TOKEN` turns it on.
    /// With `[fabric] anonymous_data_access = false`, the routes Fabric's authentication
    /// middleware guards answer requests without a token with its 401.
    pub(crate) fn routers(&self, ctx: &ServeContext<'_>) -> Vec<Router> {
        let reject_anonymous =
            ctx.project.local_config().fabric.anonymous_data_access == Some(false);
        let guarded = |router: Router| {
            if reject_anonymous {
                router.layer(axum::middleware::from_fn(rl_server::reject_anonymous))
            } else {
                router
            }
        };
        let mut routers = vec![
            Arc::clone(ctx.auth).router(),
            guarded(self.connectors.clone()),
        ];
        routers.extend(integrations::graphql_router(ctx).map(guarded));
        routers.extend(self.storage.clone().map(guarded));
        routers.extend(
            self.functions
                .get()
                .map(|service| guarded(service.router())),
        );
        routers.extend(
            ctx.control_token
                .map(|token| control_router(ctx, token, &self.functions)),
        );
        routers
    }
}

/// The host's control API (docs/embedding.md), answering `token`, over the services `ctx` has
/// and `functions`; `POST /reload` goes to live reload.
fn control_router(ctx: &ServeContext<'_>, token: &str, functions: &FunctionsSlot) -> Router {
    control::Control {
        token: token.to_owned(),
        auth: Arc::clone(ctx.auth),
        db: ctx.db.clone(),
        data: ctx.data_api.clone(),
        functions: functions.clone(),
        storage_dir: ctx.state.storage_dir(),
        reload: Arc::new(ctx.reload.clone()),
        live: events::live(),
    }
    .router()
}

/// Starts the feature services: the connectors (telling which run only on Fabric), storage and
/// the functions.
async fn start_features(ctx: &ServeContext<'_>) -> anyhow::Result<Features> {
    let connector_models = integrations::connector_models(ctx).await;
    let connectors = integrations::connectors_router(ctx, connector_models.clone());
    integrations::warn_unsupported_connectors(&ctx.project.config().connectors);
    let storage = integrations::storage_router(ctx).await?;
    let secrets = SecretValues::read(ctx);
    let functions = integrations::start_functions(ctx, &secrets).await?;
    Ok(Features {
        connectors,
        connector_models,
        storage,
        functions: FunctionsSlot::new(functions),
        secrets,
    })
}

/// The `ready` event: where the app and the API are and what runs.
pub(crate) fn ready_event(
    ctx: &ServeContext<'_>,
    features: &Features,
    app_url: Option<String>,
    auth_settings: &AuthSettings,
) -> Ready {
    let config = ctx.project.config();
    let fabric_sign_in = auth_settings.enabled && auth_settings.fabric_enabled;
    let data_enabled = ctx.data_api.is_some();
    let entities = if data_enabled {
        ctx.model
            .entities
            .iter()
            .map(|entity| entity.name.clone())
            .collect()
    } else {
        Vec::new()
    };
    let folders = if features.storage.is_some() {
        integrations::storage_folders(ctx.model)
            .into_iter()
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };
    let functions = features.functions.get();
    Ready {
        embed_url: (app_url.is_some() && fabric_sign_in)
            .then(|| format!("{}{}", ctx.api_origin, rl_auth::EMBED_HOST_PATH)),
        app_url,
        api_url: ctx.api_origin.to_owned(),
        portal_url: format!("{}{}", ctx.api_origin, rl_auth::PORTAL_PATH),
        data_dir: ctx.state.dir().display().to_string(),
        sign_in_as: SignInAs::of(&ctx.auth.sign_in_as()),
        sdk: Sdk::of(ctx.project),
        features: events::Features {
            data: DataFeature {
                enabled: data_enabled,
                entities,
            },
            functions: FunctionsFeature {
                enabled: functions.is_some(),
                names: functions
                    .as_ref()
                    .map(|service| service.functions())
                    .unwrap_or_default(),
            },
            storage: StorageFeature {
                enabled: features.storage.is_some(),
                folders,
            },
            auth: AuthFeature {
                enabled: auth_settings.enabled,
                fabric: fabric_sign_in,
                password: auth_settings.enabled && auth_settings.password_enabled,
            },
            connectors: config
                .connectors
                .iter()
                .map(|connector| ConnectorFeature {
                    name: connector.name.clone(),
                    kind: connector.connector_type.clone(),
                    supported: integrations::connector_runs_locally(&connector.connector_type),
                })
                .collect(),
        },
    }
}

/// The banner's rows: where the app and the API are, what runs, and how to sign in.
pub(crate) fn banner_rows(
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
    let entities = if ctx.data_api.is_some() {
        count(ctx.model.entities.len(), "entity", "entities")
    } else if config.services.data.enabled {
        "off (the schema change was not applied: see above)".to_owned()
    } else {
        "off (services.data is disabled in rayfin.yml)".to_owned()
    };
    let functions = match features.functions.get() {
        Some(service) => match service.functions() {
            names if names.is_empty() => "none loaded (see the function output above)".to_owned(),
            names => format!(
                "{} ({})",
                count(names.len(), "function", "functions"),
                name_list(&names)
            ),
        },
        None if ctx.options.no_functions => "off (--no-functions)".to_owned(),
        None if config.services.functions.enabled => {
            "off (they could not start: see above)".to_owned()
        }
        None => "off (services.functions is disabled in rayfin.yml)".to_owned(),
    };
    let connectors = integrations::connectors_summary(&config.connectors);
    let folders = integrations::storage_folders(ctx.model);
    let storage = if features.storage.is_some() {
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
pub(crate) fn check_anonymous_access(project: &Project, model: &AppModel) -> anyhow::Result<()> {
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
            ui::warning(
                WarningCode::AnonymousRole,
                format_args!(
                    "{subject} {verb} the anonymous role. Fabric accepts it only when a tenant \
                     administrator turns on {setting}, which is off by default; otherwise \
                     `rayfin up` refuses the schema. In rayfin-local.toml, set [fabric] \
                     anonymous_data_access to true if your tenant allows it (hides this \
                     warning), or to false to act like a tenant that doesn't."
                ),
                json!({ "entities": entities }),
            );
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

pub(crate) fn server_config(
    ctx: &ServeContext<'_>,
    static_site: Option<StaticSite>,
) -> ServerConfig {
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
