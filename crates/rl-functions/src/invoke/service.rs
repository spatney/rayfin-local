//! [`FunctionsService`]: the build, the metadata, the host and the watcher behind the routes.

use std::{
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
    time::Duration,
};

use axum::Router;
use rl_model::CallerResolver;
use rl_tokens::TokenSource;
use tokio::{sync::oneshot, task::JoinHandle};

use super::{FunctionsConfig, FunctionsError, routes, tokens::TokenFetches};
use crate::{
    builder::{self, BuildConfig, BuildError, FunctionsWatch, WatchEvent},
    host::{
        HostConfig, HostError, HostManager, ManagerOptions, RegisteredFunction,
        local_settings_values, secrets_header,
    },
    metadata,
};

/// How long [`FunctionsService::stop`] waits for the watcher (and `npm run build:watch`).
const WATCH_STOP_TIMEOUT: Duration = Duration::from_secs(15);

/// How long loading the package may take with the inspector on.
const INSPECT_READY_TIMEOUT: Duration = Duration::from_secs(600);

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
    pub(super) secrets_header: String,
    pub(super) callers: Arc<dyn CallerResolver>,
    pub(super) tokens: Arc<dyn TokenSource>,
    pub(super) token_fetches: TokenFetches,
    pub(super) manager: HostManager,
    /// Why the functions can't run (the build failed); invocations answer 503 with it.
    unavailable: Mutex<Option<String>>,
    watcher: Mutex<Option<WatchTask>>,
}

struct WatchTask {
    stop: oneshot::Sender<()>,
    handle: JoinHandle<()>,
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
        let build = BuildConfig {
            functions_dir: config.functions_dir.clone(),
            build_command: config.build_command.clone(),
            sink: config.sink.clone(),
        };

        let mut unavailable = None;
        match builder::build(&build).await {
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
        let host_config =
            host_config(&config, &api_url, entry).map_err(FunctionsError::Settings)?;
        let secrets_header = secrets_header(
            config
                .secrets
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );
        let service = Arc::new(Self {
            manager: HostManager::with_options(host_config, manager_options(&config)),
            config,
            api_url,
            secrets_header,
            callers,
            tokens,
            token_fetches: TokenFetches::default(),
            unavailable: Mutex::new(unavailable),
            watcher: Mutex::new(None),
        });

        if service.unavailable().is_none() {
            // A load failure is logged by the manager, and invocations answer it.
            if let Ok(functions) = service.manager.start().await {
                tracing::info!(
                    "Functions ready ({} functions)",
                    invocable(&functions).count()
                );
            }
        }
        match builder::watch(&build) {
            Ok(watch) => {
                let (stop, stop_rx) = oneshot::channel();
                let handle = tokio::spawn(watch_loop(Arc::downgrade(&service), watch, stop_rx));
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

    /// Why invocations can't run, while the functions are not built.
    pub(super) fn unavailable(&self) -> Option<String> {
        lock(&self.unavailable).clone()
    }

    /// New build output: refreshes the metadata and replaces the host (the UDF package reads
    /// `runtimemetadata.json` when it loads). The current host keeps running if the output has
    /// no entry point.
    async fn reload(&self) {
        refresh_metadata(&self.config).await;
        let entry = match builder::entry_point(&self.config.functions_dir) {
            Ok(entry) => entry,
            Err(error) => {
                tracing::error!("{error}. The functions were not reloaded.");
                return;
            }
        };
        let result = match host_config(&self.config, &self.api_url, entry) {
            Ok(config) => self.manager.reload_with(config).await,
            Err(error) => {
                tracing::error!(
                    "could not read the functions' local.settings.json ({error}); reloading with \
                     the previous settings"
                );
                self.manager.reload().await
            }
        };
        match result {
            Ok(functions) => {
                *lock(&self.unavailable) = None;
                tracing::info!(
                    "Functions reloaded ({} functions)",
                    invocable(&functions).count()
                );
            }
            Err(HostError::Stopped) => {}
            // The manager logged the load failure; invocations now answer it instead.
            Err(_) => *lock(&self.unavailable) = None,
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

/// Reacts to the watcher until the service stops or is dropped.
async fn watch_loop(
    service: Weak<FunctionsService>,
    mut watch: FunctionsWatch,
    mut stop: oneshot::Receiver<()>,
) {
    loop {
        let event = tokio::select! {
            _ = &mut stop => break,
            event = watch.next() => event,
        };
        let (Some(event), Some(service)) = (event, service.upgrade()) else {
            break;
        };
        match event {
            WatchEvent::OutputChanged => service.reload().await,
            WatchEvent::SourceChanged => refresh_metadata(&service.config).await,
            WatchEvent::BuildWatchExited(status) => tracing::error!(
                "`npm run {}` exited{}, so changes to the functions are no longer compiled. Fix \
                 the errors in its output (`build`) and restart rayfin-local.",
                builder::BUILD_WATCH_SCRIPT,
                status
                    .map(|status| format!(" ({status})"))
                    .unwrap_or_default()
            ),
        }
    }
    watch.stop().await;
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
    api_url: &str,
    entry: std::path::PathBuf,
) -> std::io::Result<HostConfig> {
    let mut env = local_settings_values(&config.functions_dir)?;
    let has_environment = env
        .iter()
        .any(|(name, _)| name == "AZURE_FUNCTIONS_ENVIRONMENT");
    env.extend(config.secrets.iter().cloned());
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
