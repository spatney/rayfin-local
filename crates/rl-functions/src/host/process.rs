//! One function host process.

use std::{
    ffi::OsString,
    net::Ipv4Addr,
    process::ExitStatus,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use rl_node::{LineSink, ManagedChild, NodeError, ProcessSpec, Stream};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, oneshot, watch},
};

use super::{
    HOST_SCRIPT, HostConfig, HostError, HostInvocation, HostResponse, RegisteredFunction,
    SECRET_ENV,
    wire::{self, Announcement},
};

/// How long to wait for the exit of a host whose connection broke, before calling it an I/O error.
const EXIT_SETTLE: Duration = Duration::from_secs(2);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// How a host process ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostExit {
    /// The exit code, unless a signal ended the process.
    pub code: Option<i32>,
    /// For messages, e.g. `exit code: 1`.
    pub description: String,
}

impl HostExit {
    fn from_wait(result: Result<ExitStatus, NodeError>) -> Self {
        match result {
            Ok(status) => Self {
                code: status.code(),
                description: status.to_string(),
            },
            Err(error) => Self {
                code: None,
                description: error.to_string(),
            },
        }
    }
}

/// A running `js/function-host.mjs` with the package loaded. Dropping it kills the process.
pub struct FunctionHost {
    id: u64,
    port: u16,
    secret: String,
    functions: Vec<RegisteredFunction>,
    pid: Option<u32>,
    exit: watch::Receiver<Option<HostExit>>,
    /// Asks the supervisor to kill the process tree; dropping it kills too.
    kill: mpsc::UnboundedSender<()>,
}

impl std::fmt::Debug for FunctionHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FunctionHost")
            .field("id", &self.id)
            .field("pid", &self.pid)
            .field("port", &self.port)
            .field("functions", &self.functions.len())
            .finish_non_exhaustive()
    }
}

impl FunctionHost {
    /// Starts Node.js on the functions package and waits until the package is loaded, failing
    /// with the JavaScript error when it doesn't load.
    pub async fn start(config: HostConfig) -> Result<Self, HostError> {
        let entry = config.entry_path();
        if !entry.is_file() {
            return Err(HostError::MissingEntry { entry });
        }
        let script = HOST_SCRIPT.materialize(&config.cache_dir)?;
        let secret = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );

        let mut args: Vec<OsString> = vec!["--enable-source-maps".into()];
        if let Some(port) = config.inspect_port {
            args.push(format!("--inspect=127.0.0.1:{port}").into());
        }
        args.extend(config.node_args.iter().map(OsString::from));
        args.extend([
            script.into_os_string(),
            "--entry".into(),
            entry.clone().into_os_string(),
        ]);

        let (announce_tx, mut announce_rx) = oneshot::channel();
        let mut spec = ProcessSpec::new("fn", &config.node.path, &config.functions_dir)
            .args(args)
            .sink(announcing_sink(config.sink.clone(), announce_tx));
        for (key, value) in &config.env {
            spec = spec.env(key, value);
        }
        spec = spec.env(SECRET_ENV, &secret);

        let child = ManagedChild::spawn(spec)?;
        let pid = child.pid();
        let (exit_tx, exit) = watch::channel(None);
        let (kill, kill_rx) = mpsc::unbounded_channel();
        tokio::spawn(supervise(child, kill_rx, exit_tx));

        let mut exited = exit.clone();
        let waited = tokio::time::timeout(config.ready_timeout, async {
            tokio::select! {
                biased;
                announcement = &mut announce_rx => announcement.ok(),
                _ = exited.wait_for(Option::is_some) => None,
            }
        })
        .await;
        let announcement = match waited {
            Ok(Some(announcement)) => announcement,
            // Output is delivered before the exit, but check once more for a racing line.
            Ok(None) => match announce_rx.try_recv() {
                Ok(announcement) => announcement,
                Err(_) => {
                    // The output pipes close just before the exit status is published.
                    let mut receiver = exit.clone();
                    let exit =
                        match tokio::time::timeout(EXIT_SETTLE, receiver.wait_for(Option::is_some))
                            .await
                        {
                            Ok(Ok(exit)) => exit.clone(),
                            _ => None,
                        };
                    return Err(HostError::ExitedBeforeReady {
                        exit: exit.map_or_else(|| "no exit status".into(), |e| e.description),
                    });
                }
            },
            Err(_) => return Err(HostError::StartTimeout(config.ready_timeout)),
        };

        let ready = match announcement.map_err(HostError::Protocol)? {
            Announcement::Ready(ready) => ready,
            Announcement::LoadError(error) => {
                return Err(HostError::Load {
                    entry: config.entry.display().to_string(),
                    message: match error.name {
                        Some(name) if !name.is_empty() => format!("{name}: {}", error.message),
                        _ => error.message,
                    },
                    stack: error.stack,
                });
            }
        };

        let request = wire::ControlRequest {
            kind: "describe",
            secret: &secret,
            drain_ms: None,
        };
        let reply = tokio::time::timeout(
            config.ready_timeout,
            exchange(ready.port, &to_line(&request)?),
        )
        .await
        .map_err(|_| HostError::StartTimeout(config.ready_timeout))??;
        let described: wire::Described = serde_json::from_slice(&reply)
            .map_err(|e| HostError::Protocol(format!("unreadable function list: {e}")))?;
        if !described.ok {
            return Err(HostError::Protocol(described.error.map_or_else(
                || "the host did not list its functions".into(),
                |e| e.message,
            )));
        }

        Ok(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            port: ready.port,
            secret,
            functions: described.functions,
            pid,
            exit,
            kill,
        })
    }

    /// Identifies this host among the ones a manager starts.
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Every function the package registered, HTTP or not, in registration order.
    pub fn functions(&self) -> &[RegisteredFunction] {
        &self.functions
    }

    pub fn function(&self, name: &str) -> Option<&RegisteredFunction> {
        self.functions.iter().find(|f| f.name == name)
    }

    pub fn is_alive(&self) -> bool {
        self.exit.borrow().is_none()
    }

    /// How the process ended, once it has.
    pub fn exit_status(&self) -> Option<HostExit> {
        self.exit.borrow().clone()
    }

    /// Waits for the process to end.
    pub async fn exited(&self) -> HostExit {
        let mut receiver = self.exit.clone();
        let exit = match receiver.wait_for(Option::is_some).await {
            Ok(exit) => (*exit).clone(),
            Err(_) => None,
        };
        exit.unwrap_or_else(|| HostExit {
            code: None,
            description: "the host's supervisor ended".into(),
        })
    }

    /// Runs a function: the HTTP request goes through the app's `@azure/functions` exactly as the
    /// Azure Functions worker would run it. Invocations run concurrently.
    pub async fn invoke(
        &self,
        invocation: HostInvocation,
        timeout: Duration,
    ) -> Result<HostResponse, HostError> {
        self.send(&invocation, timeout).await
    }

    pub(super) async fn send(
        &self,
        invocation: &HostInvocation,
        timeout: Duration,
    ) -> Result<HostResponse, HostError> {
        let function = &invocation.function_name;
        let registered = self
            .function(function)
            .ok_or_else(|| HostError::NotFound(function.clone()))?;
        if !registered.is_http() {
            return Err(HostError::NotInvocable {
                name: function.clone(),
                trigger: registered
                    .trigger
                    .clone()
                    .unwrap_or_else(|| "missing".into()),
            });
        }

        let (body, body_encoding) = wire::encode_body(&invocation.body);
        let request = wire::InvokeRequest {
            kind: "invoke",
            secret: &self.secret,
            invocation_id: &invocation.invocation_id,
            function_name: function,
            method: &invocation.method,
            url: invocation.url.as_deref(),
            headers: wire::header_record(&invocation.headers),
            body,
            body_encoding,
            bindings: &invocation.bindings,
        };
        let line = to_line(&request)?;

        let exchanged = match tokio::time::timeout(timeout, exchange(self.port, &line)).await {
            Ok(exchanged) => exchanged,
            Err(_) => {
                return Err(HostError::Timeout {
                    function: function.clone(),
                    timeout,
                });
            }
        };
        let reply = exchanged.map_err(HostError::from).and_then(|bytes| {
            serde_json::from_slice::<wire::Reply>(&bytes)
                .map_err(|e| HostError::Protocol(format!("unreadable reply: {e}")))
        });
        match reply {
            Ok(reply) => into_response(function, reply),
            Err(error) => {
                // No complete reply: the process is most likely gone.
                let mut receiver = self.exit.clone();
                let exit =
                    match tokio::time::timeout(EXIT_SETTLE, receiver.wait_for(Option::is_some))
                        .await
                    {
                        Ok(Ok(exit)) => exit.clone(),
                        _ => None,
                    };
                Err(match exit {
                    Some(exit) => HostError::Exited {
                        function: function.clone(),
                        exit: exit.description,
                    },
                    None => error,
                })
            }
        }
    }

    /// Stops the host: in-flight invocations get up to `grace` to finish (the host exits as soon
    /// as none is left) and `appTerminate` hooks run, then whatever is left of the process tree is
    /// killed. Invocations sent meanwhile fail with [`HostError::ShuttingDown`]. Stopping a host
    /// that is stopping again with a shorter `grace` cuts the first grace short.
    pub async fn stop(&self, grace: Duration) {
        self.begin_stop(grace).await;
        self.finish_stop(grace).await;
    }

    /// Asks the host to shut down: once this returns it refuses new invocations, and it exits as
    /// soon as those in flight are done (within `grace`).
    pub(super) async fn begin_stop(&self, grace: Duration) {
        if self.is_alive() {
            let request = wire::ControlRequest {
                kind: "shutdown",
                secret: &self.secret,
                drain_ms: Some(u64::try_from(grace.as_millis()).unwrap_or(u64::MAX)),
            };
            if let Ok(line) = to_line(&request) {
                let _ =
                    tokio::time::timeout(Duration::from_secs(2), exchange(self.port, &line)).await;
            }
        }
    }

    /// Waits for the exit [`begin_stop`](Self::begin_stop) asked for, and kills what is left of
    /// the process tree when it takes longer than `grace` (and the `appTerminate` hooks).
    pub(super) async fn finish_stop(&self, grace: Duration) {
        let mut exit = self.exit.clone();
        let deadline = grace + Duration::from_secs(3);
        if tokio::time::timeout(deadline, exit.wait_for(Option::is_some))
            .await
            .is_err()
        {
            self.kill();
        }
        self.exited().await;
    }

    /// Kills the process tree now, without waiting for it to end.
    pub fn kill(&self) {
        let _ = self.kill.send(());
    }

    pub(super) fn exit_receiver(&self) -> watch::Receiver<Option<HostExit>> {
        self.exit.clone()
    }
}

/// One request/reply exchange with the host over a fresh connection.
async fn exchange(port: u16, line: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await?;
    stream.set_nodelay(true)?;
    stream.write_all(line).await?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).await?;
    Ok(reply)
}

fn to_line(message: &impl serde::Serialize) -> Result<Vec<u8>, HostError> {
    let mut line = serde_json::to_vec(message).map_err(|e| HostError::Protocol(e.to_string()))?;
    line.push(b'\n');
    Ok(line)
}

fn into_response(function: &str, reply: wire::Reply) -> Result<HostResponse, HostError> {
    if reply.ok {
        let response = reply
            .response
            .ok_or_else(|| HostError::Protocol("a reply without a response".into()))?;
        return Ok(HostResponse {
            status: response.status,
            headers: response.headers,
            body: wire::decode_body(response.body, response.body_encoding)
                .map_err(HostError::Protocol)?,
        });
    }
    let error = reply
        .error
        .ok_or_else(|| HostError::Protocol("a failed reply without an error".into()))?;
    Err(match error.kind.as_str() {
        "notFound" => HostError::NotFound(function.to_owned()),
        "shuttingDown" => HostError::ShuttingDown,
        "notInvocable" => HostError::NotInvocable {
            name: function.to_owned(),
            trigger: error.message,
        },
        "failed" => HostError::Failed {
            function: function.to_owned(),
            error_type: error.name.unwrap_or_else(|| "Error".into()),
            message: error.message,
            stack: error.stack,
        },
        _ => HostError::Protocol(error.message),
    })
}

/// Owns the child: publishes its exit, and kills its tree when asked (or when the
/// [`FunctionHost`] is dropped).
async fn supervise(
    mut child: ManagedChild,
    mut kill: mpsc::UnboundedReceiver<()>,
    exit: watch::Sender<Option<HostExit>>,
) {
    let status = tokio::select! {
        status = child.wait() => status,
        _ = kill.recv() => {
            let _ = child.kill(Duration::ZERO).await;
            child.wait().await
        }
    };
    exit.send_replace(Some(HostExit::from_wait(status)));
}

/// Passes the host's output on to `sink`, except the announcement line, which goes to `announce`.
fn announcing_sink(
    sink: LineSink,
    announce: oneshot::Sender<Result<Announcement, String>>,
) -> LineSink {
    let announce = Mutex::new(Some(announce));
    Arc::new(move |label: &str, stream: Stream, line: &str| {
        if stream == Stream::Stdout
            && let Some((before, announcement)) = wire::split_announcement(line)
            && let Some(announce) = announce
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        {
            if !before.is_empty() {
                sink(label, stream, before);
            }
            let _ = announce.send(announcement);
            return;
        }
        sink(label, stream, line);
    })
}
