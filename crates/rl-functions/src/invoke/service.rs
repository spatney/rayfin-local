//! [`FunctionsService`]: the build, the metadata, the host and the watcher behind the routes.

use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
    time::Duration,
};

use axum::Router;
use rl_model::CallerResolver;
use rl_node::LineSink;
use rl_tokens::TokenSource;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

use super::{FunctionsConfig, FunctionsError, routes, tokens::TokenFetches};
use crate::{
    builder::{self, BuildConfig, BuildError, FunctionsWatch, WatchEvent, WatchMode},
    host::{
        HostConfig, HostError, HostManager, HostStatus, ManagerOptions, RegisteredFunction,
        local_settings_values, secrets_header,
    },
    metadata,
    state::{CompilerOutput, FunctionsState, Milestone, Reporter},
};

/// How long [`FunctionsService::stop`] waits for the watcher (and `npm run build:watch`).
const WATCH_STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// How long loading the package may take with the inspector on.
const INSPECT_READY_TIMEOUT: Duration = Duration::from_secs(600);

/// How long after a clean compilation its output may take to be seen; a compilation that
/// changed nothing has no output to reload, and is reported without a reload after this.
const SETTLE: Duration = Duration::from_secs(3);

/// The app's functions, served locally: built with the package's build command, described by a
/// current `runtimemetadata.json`, run on a supervised [`HostManager`], and rebuilt and reloaded
/// when they change. See the [module docs](super) for the routes.
///
/// A build that fails at startup does not fail [`start`](Self::start): the service starts
/// unavailable, invocations answer 503 with the build error, and the first build output the
/// watcher sees (a fixed build) brings the functions up.
pub struct FunctionsService {
    pub(super) config: FunctionsConfig,
    /// `RAYFIN_API_URL` and `rayFinEndpoint`: the origin with a trailing `/`.
    pub(super) api_url: String,
    /// The declared secrets' values (`config.secrets` at first), which
    /// [`restart_host`](Self::restart_host) replaces.
    secrets: Mutex<Secrets>,
    pub(super) callers: Arc<dyn CallerResolver>,
    pub(super) tokens: Arc<dyn TokenSource>,
    pub(super) token_fetches: TokenFetches,
    pub(super) manager: HostManager,
    /// Why the functions can't run (the build failed); invocations answer 503 with it.
    unavailable: Mutex<Option<String>>,
    watcher: Mutex<Option<WatchTask>>,
    /// Tells [`FunctionsConfig::on_state`] what the functions are doing.
    reporter: Reporter,
    /// The compiler's messages in the build's output.
    compiler: Arc<Mutex<CompilerOutput>>,
    /// The last compilation's failure (message and compiler messages), until one succeeds: a
    /// compiler that emits despite errors (`tsc`) gets the output loaded, still failed.
    compile_failure: Mutex<Option<(String, Vec<String>)>>,
    /// The command line of the build, for messages.
    build_command: String,
}

struct WatchTask {
    stop: oneshot::Sender<()>,
    handle: JoinHandle<()>,
}

/// The declared secrets' values, and the `x-ms-rayfin-secrets` header they make.
struct Secrets {
    values: Vec<(String, String)>,
    header: String,
}

impl Secrets {
    fn new(values: Vec<(String, String)>) -> Self {
        let header = secrets_header(
            values
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );
        Self { values, header }
    }
}

impl FunctionsService {
    /// Builds the functions, refreshes their metadata, starts the host (waiting until the package
    /// is loaded) and watches the package. Fails only when the package can't be served at all
    /// (no folder, no usable `package.json` or `local.settings.json`); a failed build or load is
    /// logged and answered with 503 until a rebuild fixes it.
    pub async fn start(
        mut config: FunctionsConfig,
        callers: Arc<dyn CallerResolver>,
        tokens: Arc<dyn TokenSource>,
    ) -> Result<Arc<Self>, FunctionsError> {
        let dir = std::path::absolute(&config.functions_dir)
            .unwrap_or_else(|_| config.functions_dir.clone());
        if !dir.is_dir() {
            return Err(FunctionsError::MissingDir(dir));
        }
        config.functions_dir = dir;
        // Checks that package.json names the entry point: without it nothing can be watched.
        builder::output_dirs(&config.functions_dir)?;
        let reporter = Reporter::new(config.on_state.clone());
        let compiler = Arc::new(Mutex::new(CompilerOutput::default()));
        let (milestones, mut milestones_rx) = mpsc::unbounded_channel();
        let build = BuildConfig {
            functions_dir: config.functions_dir.clone(),
            build_command: config.build_command.clone(),
            sink: compiler_sink(config.sink.clone(), Arc::clone(&compiler), milestones),
        };

        reporter.report(FunctionsState::Building);
        let mut unavailable = None;
        let built = builder::build(&build).await;
        let mut errors = Vec::new();
        while let Ok(milestone) = milestones_rx.try_recv() {
            if let Milestone::Finished { errors: found, .. } = milestone {
                errors.extend(found);
            }
        }
        errors.extend(lock(&compiler).take());
        match built {
            Ok(()) => refresh_metadata(&config).await,
            Err(error) => {
                tracing::error!("{error}");
                unavailable = Some(error.to_string());
            }
        }
        let entry = match builder::entry_point(&config.functions_dir) {
            Ok(entry) => entry,
            Err(BuildError::NotBuilt { entry, dir }) => {
                if unavailable.is_none() {
                    let error = BuildError::NotBuilt {
                        entry: entry.clone(),
                        dir,
                    };
                    tracing::error!("{error}");
                    unavailable = Some(error.to_string());
                }
                entry
            }
            Err(error) => return Err(error.into()),
        };

        let api_url = format!("{}/", config.api_origin.trim().trim_end_matches('/'));
        let secrets = Secrets::new(std::mem::take(&mut config.secrets));
        let host_config = host_config(&config, &secrets.values, &api_url, entry)
            .map_err(FunctionsError::Settings)?;
        let service = Arc::new(Self {
            manager: HostManager::with_options(host_config, manager_options(&config)),
            build_command: build.command().to_owned(),
            config,
            api_url,
            secrets: Mutex::new(secrets),
            callers,
            tokens,
            token_fetches: TokenFetches::default(),
            unavailable: Mutex::new(unavailable),
            watcher: Mutex::new(None),
            reporter,
            compiler,
            compile_failure: Mutex::new(None),
        });

        match service.unavailable() {
            Some(message) => service.report_build_failure(message, errors),
            None => {
                // A load failure is logged by the manager, and invocations answer it.
                let loaded = service.manager.start().await;
                if let Ok(functions) = &loaded {
                    tracing::info!(
                        "Functions ready ({} functions)",
                        invocable(functions).count()
                    );
                }
                service.report_loaded(&loaded);
            }
        }
        match builder::watch(&build) {
            Ok(watch) => {
                let (stop, stop_rx) = oneshot::channel();
                let handle = tokio::spawn(watch_loop(
                    Arc::downgrade(&service),
                    watch,
                    stop_rx,
                    milestones_rx,
                ));
                *lock(&service.watcher) = Some(WatchTask { stop, handle });
            }
            Err(error) => tracing::error!(
                "{error}. The functions are not rebuilt or reloaded when they change; restart \
                 rayfin-local after fixing this."
            ),
        }
        Ok(service)
    }

    /// The routes, `POST /functions/{name}/invoke` and `POST /.rayfin-local/functions/api/{name}`.
    pub fn router(self: &Arc<Self>) -> Router {
        routes::router(self.clone())
    }

    /// The functions the routes can invoke (HTTP functions that accept `POST`); empty while the
    /// host is not running.
    pub fn functions(&self) -> Vec<String> {
        self.manager
            .functions()
            .unwrap_or_default()
            .iter()
            .filter(|function| function.accepts("POST"))
            .map(|function| function.name.clone())
            .collect()
    }

    /// Stops watching (and `npm run build:watch`) and stops the host; invocations then answer
    /// 503.
    pub async fn stop(&self) {
        let task = lock(&self.watcher).take();
        let handle = task.map(|WatchTask { stop, handle }| {
            let _ = stop.send(());
            handle
        });
        self.manager.stop().await;
        if let Some(mut handle) = handle
            && tokio::time::timeout(WATCH_STOP_TIMEOUT, &mut handle)
                .await
                .is_err()
        {
            handle.abort();
        }
    }

    /// Replaces only the host, with `secrets` as the declared secrets' values and
    /// `local.settings.json` read again (the secret values changed); the build and the watcher
    /// keep running. Invocations meanwhile wait for the new host, as after a rebuild, and the
    /// outcome is reported ([`FunctionsConfig::on_state`]). While the functions are not built,
    /// the values are kept for the host a fixed build starts.
    pub async fn restart_host(&self, secrets: Vec<(String, String)>) {
        *lock(&self.secrets) = Secrets::new(secrets);
        if self.unavailable().is_none() {
            self.reload(true).await;
        }
    }

    /// The `x-ms-rayfin-secrets` header of invocations.
    pub(super) fn secrets_header(&self) -> String {
        lock(&self.secrets).header.clone()
    }

    /// Why invocations can't run, while the functions are not built.
    pub(super) fn unavailable(&self) -> Option<String> {
        lock(&self.unavailable).clone()
    }

    fn secret_values(&self) -> Vec<(String, String)> {
        lock(&self.secrets).values.clone()
    }

    /// New build output: refreshes the metadata and replaces the host (the UDF package reads
    /// `runtimemetadata.json` when it loads). The current host keeps running if the output has
    /// no entry point. `report`: tell the observer the outcome (not while a compilation that
    /// will report it runs).
    async fn reload(&self, report: bool) {
        refresh_metadata(&self.config).await;
        let entry = match builder::entry_point(&self.config.functions_dir) {
            Ok(entry) => entry,
            Err(error) => {
                tracing::error!("{error}. The functions were not reloaded.");
                self.report_build_failure(
                    format!("{error}. The functions were not reloaded."),
                    Vec::new(),
                );
                return;
            }
        };
        let config = host_config(&self.config, &self.secret_values(), &self.api_url, entry);
        let result = match config {
            Ok(config) => self.manager.reload_with(config).await,
            Err(error) => {
                tracing::error!(
                    "could not read the functions' local.settings.json ({error}); reloading with \
                     the previous settings"
                );
                self.manager.reload().await
            }
        };
        match &result {
            Ok(functions) => {
                *lock(&self.unavailable) = None;
                tracing::info!(
                    "Functions reloaded ({} functions)",
                    invocable(functions).count()
                );
            }
            Err(HostError::Stopped) => {}
            // The manager logged the load failure; invocations now answer it instead.
            Err(_) => *lock(&self.unavailable) = None,
        }
        if report {
            self.report_loaded(&result);
        }
    }

    /// What loading the package (a start or a reload) came to; output loaded despite a failed
    /// compilation is still failed.
    fn report_loaded(&self, loaded: &Result<Vec<RegisteredFunction>, HostError>) {
        match loaded {
            Ok(functions) => {
                let names = invocable(functions)
                    .map(|function| function.name.clone())
                    .collect();
                let state = match lock(&self.compile_failure).clone() {
                    Some((message, errors)) => FunctionsState::Failed {
                        message,
                        errors,
                        build: true,
                        names,
                    },
                    None => FunctionsState::Ready { names },
                };
                self.reporter.report(state);
            }
            Err(HostError::Stopped) => {}
            Err(error) => self.reporter.report(FunctionsState::Failed {
                message: error.to_string(),
                errors: Vec::new(),
                build: false,
                names: Vec::new(),
            }),
        }
    }

    /// A failed build; the previous build's functions (if any) keep running.
    fn report_build_failure(&self, message: String, errors: Vec<String>) {
        *lock(&self.compile_failure) = Some((message.clone(), errors.clone()));
        self.reporter.report(FunctionsState::Failed {
            message,
            errors,
            build: true,
            names: self.functions(),
        });
    }

    /// A compilation without errors.
    fn compiled(&self) {
        *lock(&self.compile_failure) = None;
    }

    /// After a compilation that left the output as it was, or whose output was loaded while it
    /// ran: the functions that run, or a first host for a package whose earlier build failed.
    async fn settled(&self) {
        if self.unavailable().is_some() {
            self.reload(true).await;
            return;
        }
        match self.manager.status() {
            HostStatus::Ready { functions } => self.report_loaded(&Ok(functions)),
            HostStatus::Failed(error) => self.report_loaded(&Err(error)),
            HostStatus::Idle | HostStatus::Starting | HostStatus::Stopped => {}
        }
    }
}

impl std::fmt::Debug for FunctionsService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FunctionsService")
            .field("config", &self.config)
            .field("unavailable", &self.unavailable())
            .finish_non_exhaustive()
    }
}

/// Passes the build's output on to `sink`, following the compiler's messages in it.
fn compiler_sink(
    sink: LineSink,
    compiler: Arc<Mutex<CompilerOutput>>,
    milestones: mpsc::UnboundedSender<Milestone>,
) -> LineSink {
    Arc::new(move |label, stream, line| {
        if label == builder::LABEL
            && let Some(milestone) = lock(&compiler).line(line)
        {
            let _ = milestones.send(milestone);
        }
        sink(label, stream, line);
    })
}

/// What the watch loop waits for.
enum Next {
    Watch(Option<WatchEvent>),
    Milestone(Milestone),
    Settled,
}

/// Reacts to the watcher until the service stops or is dropped. `npm run build:watch`
/// (`tsc --watch`) reports its compilations in its output (`milestones`); without it the watcher
/// rebuilds after source changes and reports the outcome. A compilation's outcome decides what
/// is reported: output it emits is loaded while it runs, but reported once it ends.
async fn watch_loop(
    service: Weak<FunctionsService>,
    mut watch: FunctionsWatch,
    mut stop: oneshot::Receiver<()>,
    mut milestones: mpsc::UnboundedReceiver<Milestone>,
) {
    let rebuilds = watch.mode() == WatchMode::Rebuild;
    // The compiler's messages of the rebuild in progress, from a summary line in its output.
    let mut rebuild_errors = Vec::new();
    let mut compiling = false;
    // Whether output was loaded while the compilation ran.
    let mut reloaded = false;
    let mut settle_at: Option<Instant> = None;
    loop {
        // Milestones first: a compilation prints its outcome before its output settles.
        let next = tokio::select! {
            biased;
            _ = &mut stop => break,
            Some(milestone) = milestones.recv() => Next::Milestone(milestone),
            event = watch.next() => Next::Watch(event),
            () = sleep_until(settle_at) => Next::Settled,
        };
        let Some(service) = service.upgrade() else {
            break;
        };
        match next {
            Next::Watch(None) => break,
            Next::Watch(Some(WatchEvent::OutputChanged)) => {
                service.reload(!compiling).await;
                if compiling {
                    reloaded = true;
                    // In case the compilation's end is never seen.
                    settle_at = Some(Instant::now() + SETTLE);
                } else {
                    settle_at = None;
                }
            }
            Next::Watch(Some(WatchEvent::SourceChanged)) => {
                if rebuilds {
                    (compiling, reloaded, settle_at) = (true, false, None);
                    service.reporter.report(FunctionsState::Building);
                }
                refresh_metadata(&service.config).await;
            }
            Next::Watch(Some(WatchEvent::Rebuilt { ok })) => {
                compiling = false;
                let mut errors = std::mem::take(&mut rebuild_errors);
                errors.extend(lock(&service.compiler).take());
                if ok {
                    service.compiled();
                    settle_at = settle(&service, std::mem::take(&mut reloaded), true).await;
                } else {
                    settle_at = None;
                    let message = format!(
                        "functions build failed: `{}` failed in {}",
                        service.build_command,
                        service.config.functions_dir.display()
                    );
                    service.report_build_failure(message, errors);
                }
            }
            Next::Watch(Some(WatchEvent::BuildWatchExited(status))) => {
                compiling = false;
                let message = format!(
                    "`npm run {}` exited{}, so changes to the functions are no longer compiled. \
                     Fix the errors in its output (`build`) and restart rayfin-local.",
                    builder::BUILD_WATCH_SCRIPT,
                    status
                        .map(|status| format!(" ({status})"))
                        .unwrap_or_default()
                );
                tracing::error!("{message}");
                service.report_build_failure(message, Vec::new());
            }
            Next::Milestone(Milestone::Finished { errors, .. }) if rebuilds => {
                rebuild_errors.extend(errors);
            }
            Next::Milestone(Milestone::Started { announce }) => {
                (compiling, reloaded, settle_at) = (true, false, None);
                if announce {
                    service.reporter.report(FunctionsState::Building);
                }
            }
            Next::Milestone(Milestone::Finished { count: 0, .. }) => {
                compiling = false;
                service.compiled();
                let waiting = matches!(
                    service.reporter.last(),
                    Some(FunctionsState::Building | FunctionsState::Failed { build: true, .. })
                );
                settle_at = settle(&service, std::mem::take(&mut reloaded), waiting).await;
            }
            Next::Milestone(Milestone::Finished { count, errors }) => {
                (compiling, reloaded, settle_at) = (false, false, None);
                // The first compilation of `tsc --watch` repeats a failed build's errors.
                let repeated = matches!(
                    service.reporter.last(),
                    Some(FunctionsState::Failed { build: true, errors: reported, .. })
                        if reported == errors
                );
                if !repeated {
                    let message = format!(
                        "functions build failed: `npm run {}` found {count} {}",
                        builder::BUILD_WATCH_SCRIPT,
                        if count == 1 { "error" } else { "errors" }
                    );
                    service.report_build_failure(message, errors);
                }
            }
            Next::Settled => {
                (compiling, reloaded, settle_at) = (false, false, None);
                service.settled().await;
            }
        }
    }
    watch.stop().await;
}

/// After a clean compilation: reports what runs now when its output was loaded while it ran;
/// else, when a report is `waiting` for it, when to report without its output (it may have left
/// the output as it was).
async fn settle(service: &FunctionsService, reloaded: bool, waiting: bool) -> Option<Instant> {
    if reloaded {
        service.settled().await;
        None
    } else {
        waiting.then(|| Instant::now() + SETTLE)
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn refresh_metadata(config: &FunctionsConfig) {
    let refreshed = metadata::ensure_fresh(
        &config.node,
        &config.functions_dir,
        &config.cache_dir,
        config.sink.clone(),
    )
    .await;
    if let Err(error) = refreshed {
        tracing::error!("{error}");
    }
}

/// The host's config: `local.settings.json` `Values`, the declared secrets, then the local
/// backend's settings (see the module docs).
fn host_config(
    config: &FunctionsConfig,
    secrets: &[(String, String)],
    api_url: &str,
    entry: std::path::PathBuf,
) -> std::io::Result<HostConfig> {
    let mut env = local_settings_values(&config.functions_dir)?;
    let has_environment = env
        .iter()
        .any(|(name, _)| name == "AZURE_FUNCTIONS_ENVIRONMENT");
    env.extend(secrets.iter().cloned());
    if !has_environment {
        env.push(("AZURE_FUNCTIONS_ENVIRONMENT".into(), "Development".into()));
    }
    for (name, value) in [
        ("RAYFIN_API_URL", api_url),
        ("RAYFIN_PUBLISHABLE_KEY", &config.publishable_key),
        ("RAYFIN_FABRIC_WORKSPACE_ID", &config.workspace_id),
        ("RAYFIN_FABRIC_ITEM_ID", &config.item_id),
        ("IsLocal", "false"),
        ("RAYFIN_ACCESS_TOKEN", ""),
    ] {
        env.push((name.into(), value.into()));
    }
    let mut host = HostConfig::new(
        config.node.clone(),
        &config.functions_dir,
        entry,
        &config.cache_dir,
        config.sink.clone(),
    );
    host.env = env;
    host.inspect_port = config.inspect_port;
    if config.inspect_port.is_some() {
        // A breakpoint in the package's startup code holds the load.
        host.ready_timeout = INSPECT_READY_TIMEOUT;
    }
    Ok(host)
}

/// A replaced host may finish its invocations within their timeout. With the inspector, a
/// timed-out invocation does not replace the host: it may be paused at a breakpoint, and the
/// debug session would end with the host.
fn manager_options(config: &FunctionsConfig) -> ManagerOptions {
    ManagerOptions {
        drain_timeout: config.invoke_timeout,
        restart_on_timeout: config.inspect_port.is_none(),
        ..ManagerOptions::default()
    }
}

fn invocable(functions: &[RegisteredFunction]) -> impl Iterator<Item = &RegisteredFunction> {
    functions.iter().filter(|function| function.accepts("POST"))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
