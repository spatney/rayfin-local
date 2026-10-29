//! Keeping one function host available: started on demand, restarted after crashes, timeouts and
//! rebuilds, and stopped in order.

use std::{
    sync::{Arc, Mutex, PoisonError, Weak},
    time::Duration,
};

use tokio::{sync::watch, time::Instant};

use super::{
    FunctionHost, HostConfig, HostError, HostInvocation, HostResponse, RegisteredFunction,
};

/// Uptime after which a crash no longer counts towards the restart backoff.
const STABLE_UPTIME: Duration = Duration::from_secs(60);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);

/// Knobs for [`HostManager`].
#[derive(Clone, Debug)]
pub struct ManagerOptions {
    /// How long an invocation waits for a (re)starting host before [`HostError::Starting`].
    pub start_wait: Duration,
    /// How long a host being stopped may finish its in-flight invocations: on
    /// [`HostManager::stop`], and on a reload with [`HostConfig::inspect_port`] set (the old host
    /// must free the port before the new one starts).
    pub stop_grace: Duration,
    /// How long a host replaced by a reload may finish its in-flight invocations, while the new
    /// host takes the new ones; it exits as soon as they are done. Give it the invocation timeout.
    pub drain_timeout: Duration,
    /// Replace the host after an invocation times out, as the Functions host does: the timed-out
    /// code may still be running, or blocking the event loop.
    pub restart_on_timeout: bool,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            start_wait: Duration::from_secs(30),
            stop_grace: Duration::from_secs(5),
            drain_timeout: crate::invoke::DEFAULT_INVOKE_TIMEOUT,
            restart_on_timeout: true,
        }
    }
}

/// What a [`HostManager`] is doing.
#[derive(Clone, Debug)]
pub enum HostStatus {
    /// Not started yet, or between a crash and the restart.
    Idle,
    Starting,
    Ready {
        functions: Vec<RegisteredFunction>,
    },
    /// The package failed to load; invocations fail with this error until a reload or start.
    Failed(HostError),
    Stopped,
}

#[derive(Clone)]
enum State {
    Idle,
    Starting,
    Ready(Arc<FunctionHost>),
    Failed(HostError),
    Stopped,
}

/// Runs the functions package on one [`FunctionHost`] at a time.
///
/// The host starts on [`start`](Self::start) or the first invocation. A host that exits on its
/// own is restarted (immediately, then with a backoff while it keeps crashing within a minute of
/// starting), and a host whose invocation timed out is replaced. [`reload`](Self::reload) (after
/// a rebuild) starts a new host and lets the old one finish its invocations, within
/// [`ManagerOptions::drain_timeout`]. Starts, reloads and stops run one at a time; invocations
/// meanwhile wait up to [`ManagerOptions::start_wait`] for the host, then fail with
/// [`HostError::Starting`]. A package that fails to load stays failed until the next reload or
/// start; a start that failed for a reason that may pass ([`HostError::is_transient`]) is tried
/// again by the next invocation.
#[derive(Clone)]
pub struct HostManager {
    shared: Arc<Shared>,
}

struct Shared {
    config: Mutex<HostConfig>,
    options: ManagerOptions,
    state: watch::Sender<State>,
    /// Serializes starts, reloads and stops.
    lifecycle: tokio::sync::Mutex<()>,
    /// Crashes in a row of hosts that ran for less than [`STABLE_UPTIME`].
    quick_crashes: Mutex<u32>,
    /// Hosts replaced by a reload that are finishing their invocations.
    draining: Mutex<Vec<Arc<FunctionHost>>>,
}

/// The settled states [`Shared::request_start`] leaves for a new start.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StartFrom {
    /// `Idle`, or a `Ready` whose host died.
    Idle,
    /// Also a start that failed for a reason that may pass.
    TransientFailure,
    /// Also any failed start, and `Stopped`.
    Any,
}

impl HostManager {
    pub fn new(config: HostConfig) -> Self {
        Self::with_options(config, ManagerOptions::default())
    }

    pub fn with_options(config: HostConfig, options: ManagerOptions) -> Self {
        Self {
            shared: Arc::new(Shared {
                config: Mutex::new(config),
                options,
                state: watch::Sender::new(State::Idle),
                lifecycle: tokio::sync::Mutex::new(()),
                quick_crashes: Mutex::new(0),
                draining: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Starts the host unless it is running or starting, and waits until the package is loaded.
    /// Also retries after a failed load, and restarts a stopped manager.
    pub async fn start(&self) -> Result<Vec<RegisteredFunction>, HostError> {
        self.shared.request_start(StartFrom::Any);
        self.shared.settled().await
    }

    /// Replaces the host with a new one on the current config (e.g. after the package was
    /// rebuilt), and waits until it is loaded. The old host finishes its in-flight invocations in
    /// the background (within [`ManagerOptions::drain_timeout`]); with
    /// [`HostConfig::inspect_port`] set it does so (within [`ManagerOptions::stop_grace`]) before
    /// the new host starts, which then gets the inspector's port.
    pub async fn reload(&self) -> Result<Vec<RegisteredFunction>, HostError> {
        self.shared.clone().reload(None).await
    }

    /// Like [`reload`](Self::reload), with a new config (e.g. changed settings).
    pub async fn reload_with(
        &self,
        config: HostConfig,
    ) -> Result<Vec<RegisteredFunction>, HostError> {
        self.shared.clone().reload(Some(config)).await
    }

    /// Stops the host and the replaced hosts still finishing invocations (in-flight invocations
    /// get [`ManagerOptions::stop_grace`]), and keeps it stopped until [`start`](Self::start).
    pub async fn stop(&self) {
        let previous = self.shared.state.send_replace(State::Stopped);
        // A start in progress sees `Stopped` when it finishes and stops its host.
        let _lifecycle = self.shared.lifecycle.lock().await;
        let mut hosts = self
            .shared
            .draining
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let State::Ready(host) = previous {
            hosts.push(host);
        }
        let grace = self.shared.options.stop_grace;
        let mut stopping = tokio::task::JoinSet::new();
        for host in hosts {
            stopping.spawn(async move { host.stop(grace).await });
        }
        while stopping.join_next().await.is_some() {}
    }

    pub fn status(&self) -> HostStatus {
        match &*self.shared.state.borrow() {
            State::Idle => HostStatus::Idle,
            State::Starting => HostStatus::Starting,
            State::Ready(host) => HostStatus::Ready {
                functions: host.functions().to_vec(),
            },
            State::Failed(error) => HostStatus::Failed(error.clone()),
            State::Stopped => HostStatus::Stopped,
        }
    }

    /// The functions of the running host.
    pub fn functions(&self) -> Option<Vec<RegisteredFunction>> {
        match &*self.shared.state.borrow() {
            State::Ready(host) => Some(host.functions().to_vec()),
            _ => None,
        }
    }

    /// The running host, started if need be (waiting as invocations do). Look a function up on
    /// it (e.g. for [`RegisteredFunction::audiences`]) and run it with
    /// [`invoke_on`](Self::invoke_on).
    pub async fn host(&self) -> Result<Arc<FunctionHost>, HostError> {
        self.shared.ready_host().await
    }

    /// Runs a function on the host, starting it if need be (see [`FunctionHost::invoke`]).
    pub async fn invoke(
        &self,
        invocation: HostInvocation,
        timeout: Duration,
    ) -> Result<HostResponse, HostError> {
        let host = self.shared.ready_host().await?;
        self.invoke_on(&host, invocation, timeout).await
    }

    /// Runs a function on `host` (from [`host`](Self::host)); a timeout replaces the host. When
    /// `host` was replaced since (a reload) and refuses new invocations, the function runs on the
    /// current host instead.
    pub async fn invoke_on(
        &self,
        host: &Arc<FunctionHost>,
        invocation: HostInvocation,
        timeout: Duration,
    ) -> Result<HostResponse, HostError> {
        let result = self.shared.invoke_on(host, &invocation, timeout).await;
        if !matches!(result, Err(HostError::ShuttingDown)) {
            return result;
        }
        let current = self.shared.ready_host().await?;
        self.shared.invoke_on(&current, &invocation, timeout).await
    }
}

impl Shared {
    async fn invoke_on(
        self: &Arc<Self>,
        host: &Arc<FunctionHost>,
        invocation: &HostInvocation,
        timeout: Duration,
    ) -> Result<HostResponse, HostError> {
        let result = host.send(invocation, timeout).await;
        if let Err(HostError::Timeout { function, timeout }) = &result
            && self.options.restart_on_timeout
        {
            self.retire(
                host,
                &format!("'{function}' did not finish within {timeout:?}"),
            );
        }
        result
    }

    fn config(&self) -> HostConfig {
        self.config
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Moves a state `from` allows (see [`StartFrom`]) to `Starting` and starts a host in the
    /// background.
    fn request_start(self: &Arc<Self>, from: StartFrom) {
        let requested = self.state.send_if_modified(|state| {
            let start = match state {
                State::Idle => true,
                State::Ready(host) => !host.is_alive(),
                State::Failed(error) => {
                    from == StartFrom::Any
                        || (from == StartFrom::TransientFailure && error.is_transient())
                }
                State::Stopped => from == StartFrom::Any,
                State::Starting => false,
            };
            if start {
                *state = State::Starting;
            }
            start
        });
        if requested {
            // Spawned, so that a caller giving up does not abandon the start halfway.
            tokio::spawn(self.clone().run_start());
        }
    }

    async fn run_start(self: Arc<Self>) {
        let _lifecycle = self.lifecycle.lock().await;
        if !matches!(*self.state.borrow(), State::Starting) {
            return;
        }
        let result = FunctionHost::start(self.config()).await;
        let _ = self.install(result).await;
    }

    /// Publishes a start's outcome, unless the manager was stopped meanwhile (then the new host
    /// is stopped). Call with the lifecycle lock held.
    async fn install(
        self: &Arc<Self>,
        result: Result<FunctionHost, HostError>,
    ) -> Result<Vec<RegisteredFunction>, HostError> {
        match result {
            Ok(host) => {
                let host = Arc::new(host);
                let functions = host.functions().to_vec();
                let installed = self.state.send_if_modified(|state| {
                    let starting = matches!(state, State::Starting);
                    if starting {
                        *state = State::Ready(host.clone());
                    }
                    starting
                });
                if !installed {
                    host.stop(self.options.stop_grace).await;
                    return Err(HostError::Stopped);
                }
                tracing::info!(
                    pid = host.pid(),
                    functions = functions.len(),
                    "function host ready"
                );
                self.watch_exit(&host);
                Ok(functions)
            }
            Err(error) => {
                match &error {
                    HostError::Load {
                        stack: Some(stack), ..
                    } => tracing::error!("{error}\n{stack}"),
                    _ => tracing::error!("{error}"),
                }
                self.state.send_if_modified(|state| {
                    let starting = matches!(state, State::Starting);
                    if starting {
                        *state = State::Failed(error.clone());
                    }
                    starting
                });
                Err(error)
            }
        }
    }

    async fn reload(
        self: Arc<Self>,
        config: Option<HostConfig>,
    ) -> Result<Vec<RegisteredFunction>, HostError> {
        let _lifecycle = self.lifecycle.lock().await;
        if let Some(config) = config {
            *self.config.lock().unwrap_or_else(PoisonError::into_inner) = config;
        }
        let mut previous = None;
        let reloading = self.state.send_if_modified(|state| {
            if matches!(state, State::Stopped) {
                return false;
            }
            previous = Some(std::mem::replace(state, State::Starting));
            true
        });
        if !reloading {
            return Err(HostError::Stopped);
        }
        let config = self.config();
        // Two hosts can't share the inspector's port: with one, the old host goes first.
        if config.inspect_port.is_some()
            && let Some(State::Ready(old)) = previous.take()
        {
            old.stop(self.options.stop_grace).await;
        }
        let result = FunctionHost::start(config).await;
        let installed = self.install(result).await;
        if let Some(State::Ready(old)) = previous {
            self.drain(old).await;
        }
        installed
    }

    /// Stops a host replaced by a reload: it refuses new invocations once this returns (which
    /// [`HostManager::invoke_on`] then runs on the current host), and finishes those in flight in
    /// the background, within [`ManagerOptions::drain_timeout`].
    async fn drain(self: &Arc<Self>, host: Arc<FunctionHost>) {
        self.draining
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(host.clone());
        let timeout = self.options.drain_timeout;
        host.begin_stop(timeout).await;
        let shared = Arc::downgrade(self);
        tokio::spawn(async move {
            host.finish_stop(timeout).await;
            if let Some(shared) = shared.upgrade() {
                shared
                    .draining
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .retain(|draining| draining.id() != host.id());
            }
        });
    }

    /// Waits until a start has an outcome.
    async fn settled(&self) -> Result<Vec<RegisteredFunction>, HostError> {
        let mut state = self.state.subscribe();
        loop {
            match &*state.borrow_and_update() {
                State::Ready(host) => return Ok(host.functions().to_vec()),
                State::Failed(error) => return Err(error.clone()),
                State::Stopped => return Err(HostError::Stopped),
                State::Idle | State::Starting => {}
            }
            if state.changed().await.is_err() {
                return Err(HostError::Stopped);
            }
        }
    }

    /// A live host, starting one if need be; waits up to `start_wait` for a start. A start that
    /// failed before for a reason that may pass is tried again, once.
    async fn ready_host(self: &Arc<Self>) -> Result<Arc<FunctionHost>, HostError> {
        let mut state = self.state.subscribe();
        let deadline = Instant::now() + self.options.start_wait;
        let mut may_retry = true;
        loop {
            let start = match &*state.borrow_and_update() {
                State::Ready(host) if host.is_alive() => return Ok(host.clone()),
                State::Ready(_) | State::Idle => Some(StartFrom::Idle),
                State::Starting => None,
                State::Failed(error) if error.is_transient() && may_retry => {
                    Some(StartFrom::TransientFailure)
                }
                State::Failed(error) => return Err(error.clone()),
                State::Stopped => return Err(HostError::Stopped),
            };
            may_retry = false;
            if let Some(from) = start {
                self.request_start(from);
            }
            match tokio::time::timeout_at(deadline, state.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(HostError::Stopped),
                Err(_) => return Err(HostError::Starting),
            }
        }
    }

    /// Kills `host` and starts a new one, if `host` is still the current one.
    fn retire(self: &Arc<Self>, host: &Arc<FunctionHost>, reason: &str) {
        let retired = self.state.send_if_modified(|state| {
            let current = matches!(state, State::Ready(h) if h.id() == host.id());
            if current {
                *state = State::Idle;
            }
            current
        });
        if retired {
            tracing::warn!("{reason}; restarting the function host");
            host.kill();
            self.request_start(StartFrom::Idle);
        }
    }

    /// Restarts the host when it exits on its own.
    fn watch_exit(self: &Arc<Self>, host: &Arc<FunctionHost>) {
        let weak: Weak<Self> = Arc::downgrade(self);
        let id = host.id();
        let mut exit = host.exit_receiver();
        let started = Instant::now();
        tokio::spawn(async move {
            let description = match exit.wait_for(Option::is_some).await {
                Ok(status) => (*status).as_ref().map(|e| e.description.clone()),
                Err(_) => None,
            }
            .unwrap_or_else(|| "no exit status".into());
            let delay = {
                let Some(shared) = weak.upgrade() else {
                    return;
                };
                let crashed = shared.state.send_if_modified(|state| {
                    let current = matches!(state, State::Ready(h) if h.id() == id);
                    if current {
                        *state = State::Idle;
                    }
                    current
                });
                if !crashed {
                    return;
                }
                shared.restart_delay(started.elapsed())
            };
            tracing::warn!(
                "the function host exited unexpectedly ({description}); restarting it{}",
                if delay.is_zero() {
                    String::new()
                } else {
                    format!(" in {delay:?}")
                }
            );
            tokio::time::sleep(delay).await;
            if let Some(shared) = weak.upgrade() {
                shared.request_start(StartFrom::Idle);
            }
        });
    }

    fn restart_delay(&self, uptime: Duration) -> Duration {
        let mut quick_crashes = self
            .quick_crashes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if uptime >= STABLE_UPTIME {
            *quick_crashes = 0;
        }
        *quick_crashes += 1;
        match *quick_crashes {
            1 => Duration::ZERO,
            n => Duration::from_secs(1 << (n - 2).min(5)).min(MAX_RESTART_DELAY),
        }
    }
}
