//! The app's own Vite dev server, run by `js/vite-dev.mjs`.
//!
//! The helper's protocol (the script's header has the details):
//! - `node vite-dev.mjs --root <frontend dir> --host <host> --port <port> --backend <origin>`, in
//!   the frontend dir, with the Rayfin env set on the process.
//! - stdout `@@rayfin-local:vite-ready {"urls", "version", "rayfinPlugin", ...}` once Vite
//!   listens, or `@@rayfin-local:vite-error {"code", "message", "stack"}` before it exits 1. These
//!   lines are not passed on to the sink.
//! - Ctrl+Break (Windows) or SIGTERM (Unix), as [`ManagedChild::kill`] sends them, close the server.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rl_node::{Helper, LineSink, ManagedChild, NodeRuntime, ProcessSpec, Stream};
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::FrontendError;

const HELPER: Helper = Helper::new("vite-dev", include_str!("../../../js/vite-dev.mjs"));
const LABEL: &str = "vite";
const MARKER: &str = "@@rayfin-local:";
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long Vite gets to close before its process tree is killed.
const STOP_GRACE: Duration = Duration::from_secs(3);
/// How many of Vite's last output lines an early-exit error quotes.
const TAIL_LINES: usize = 12;

/// How to run the app's Vite dev server.
pub struct DevServerConfig {
    pub node: NodeRuntime,
    /// The folder with the app's `package.json` and Vite config (`services.staticHosting.path`,
    /// else the project root).
    pub frontend_dir: PathBuf,
    /// Where the helper script is written (`<project>/.rayfin-local/cache`).
    pub cache_dir: PathBuf,
    /// The address Vite listens on, such as `localhost` or `127.0.0.1`.
    pub host: String,
    /// Vite fails rather than move to another port (`strictPort`); 0 picks a free one.
    pub port: u16,
    /// The backend origin, such as `http://localhost:7171`, which `/.well-known/*` and
    /// `/.rayfin-local/*` are proxied to (and, for apps without `rayfinLocalDev()`, the routes
    /// the SDK calls).
    pub backend_origin: String,
    /// The Rayfin env from [`crate::frontend_env`].
    pub env: Vec<(String, String)>,
    /// Receives Vite's output, labeled `vite`.
    pub sink: LineSink,
}

impl fmt::Debug for DevServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DevServerConfig")
            .field("node", &self.node)
            .field("frontend_dir", &self.frontend_dir)
            .field("cache_dir", &self.cache_dir)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("backend_origin", &self.backend_origin)
            .field(
                "env",
                &self.env.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// The running Vite dev server. Dropping it kills Vite's process tree.
pub struct DevServer {
    child: ManagedChild,
    /// Never empty: the local URLs first, as Vite prints them.
    urls: Vec<String>,
    vite_version: String,
    rayfin_plugin: bool,
}

impl fmt::Debug for DevServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DevServer")
            .field("pid", &self.child.pid())
            .field("urls", &self.urls)
            .field("vite_version", &self.vite_version)
            .field("rayfin_plugin", &self.rayfin_plugin)
            .finish_non_exhaustive()
    }
}

/// What the helper announced.
#[derive(Debug)]
enum Announcement {
    Ready(Ready),
    Error(StartFailure),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Ready {
    urls: Vec<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    rayfin_plugin: bool,
}

/// Why Vite could not start: a Node.js error `code` (or the helper's own, `VITE_NOT_FOUND`).
#[derive(Debug, Deserialize)]
struct StartFailure {
    #[serde(default)]
    code: Option<String>,
    message: String,
    #[serde(default)]
    stack: Option<String>,
}

/// The sink's state until the helper announces itself.
struct Startup {
    announce: Option<oneshot::Sender<Result<Announcement, String>>>,
    tail: VecDeque<String>,
}

enum Waited {
    Announced(Option<Result<Announcement, String>>),
    Exited,
}

impl DevServer {
    /// Starts the app's Vite dev server and waits until it listens (up to 60 seconds).
    pub async fn start(config: DevServerConfig) -> Result<Self, FrontendError> {
        let frontend_dir = dunce::simplified(&config.frontend_dir).to_path_buf();
        if !frontend_dir.is_dir() {
            return Err(FrontendError::MissingFrontendDir(frontend_dir));
        }
        if !vite_installed(&frontend_dir) {
            return Err(FrontendError::ViteNotInstalled {
                hint: install_hint(&frontend_dir),
                dir: frontend_dir,
            });
        }
        if !port_is_free(&config.host, config.port) {
            return Err(FrontendError::PortInUse { port: config.port });
        }
        let script = HELPER.materialize(&config.cache_dir)?;

        let (announce, mut announced) = oneshot::channel();
        let startup = Arc::new(Mutex::new(Startup {
            announce: Some(announce),
            tail: VecDeque::with_capacity(TAIL_LINES),
        }));
        let args: [OsString; 8] = [
            "--root".into(),
            frontend_dir.clone().into(),
            "--host".into(),
            config.host.clone().into(),
            "--port".into(),
            config.port.to_string().into(),
            "--backend".into(),
            config.backend_origin.trim_end_matches('/').into(),
        ];
        let mut spec = ProcessSpec::node(LABEL, &config.node, &script, &frontend_dir)
            .args(args)
            .sink(announcing_sink(config.sink, Arc::clone(&startup)));
        if crate::color_wanted() {
            spec = spec.env("FORCE_COLOR", "1");
        }
        for (name, value) in &config.env {
            spec = spec.env(name, value);
        }
        if config.port != 0 {
            // As `rayfin dev` does; template configs pin `server.port` to `VITE_PORT`.
            let port = config.port.to_string();
            spec = spec
                .env("RAYFIN_PUBLIC_FRONTEND_PORT", &port)
                .env("VITE_PORT", &port);
        }

        tracing::debug!(dir = %frontend_dir.display(), port = config.port, "starting Vite");
        let mut child = ManagedChild::spawn(spec)?;
        let waited = tokio::time::timeout(READY_TIMEOUT, async {
            tokio::select! {
                biased;
                announcement = &mut announced => Waited::Announced(announcement.ok()),
                _ = child.wait() => Waited::Exited,
            }
        })
        .await;
        let announcement = match waited {
            Ok(Waited::Announced(Some(announcement))) => announcement,
            // `wait` returns once the output is read, so an announcement made before the exit is
            // in the channel by now.
            Ok(Waited::Announced(None) | Waited::Exited) => match announced.try_recv() {
                Ok(announcement) => announcement,
                Err(_) => {
                    let status = child.wait().await?;
                    let tail = std::mem::take(
                        &mut startup.lock().unwrap_or_else(PoisonError::into_inner).tail,
                    );
                    let output = tail.iter().map(|line| format!("\n  {line}")).collect();
                    return Err(FrontendError::ExitedBeforeReady {
                        status: status.to_string(),
                        output,
                        dir: frontend_dir,
                    });
                }
            },
            Err(_) => {
                let _ = child.kill(STOP_GRACE).await;
                return Err(FrontendError::StartTimeout(READY_TIMEOUT));
            }
        };

        let ready = match announcement {
            Ok(Announcement::Ready(ready)) => ready,
            Ok(Announcement::Error(failure)) => {
                let _ = child.kill(STOP_GRACE).await;
                return Err(start_error(failure, config.port, &frontend_dir));
            }
            Err(message) => {
                let _ = child.kill(STOP_GRACE).await;
                return Err(FrontendError::Protocol(message));
            }
        };
        let mut urls = ready.urls;
        if urls.is_empty() {
            urls.push(fallback_url(&config.host, config.port));
        }
        let vite_version = ready.version.unwrap_or_default();
        tracing::debug!(
            url = %urls[0],
            vite = %vite_version,
            rayfin_plugin = ready.rayfin_plugin,
            "Vite is ready"
        );
        Ok(Self {
            child,
            urls,
            vite_version,
            rayfin_plugin: ready.rayfin_plugin,
        })
    }

    /// The app's URL, as Vite prints it (such as `http://localhost:5175/`).
    pub fn url(&self) -> &str {
        &self.urls[0]
    }

    /// Every URL Vite answers on: the local ones, then the network ones.
    pub fn urls(&self) -> &[String] {
        &self.urls
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.pid()
    }

    /// The version of the app's `vite` package.
    pub fn vite_version(&self) -> &str {
        &self.vite_version
    }

    /// Whether the app's Vite config has `rayfinLocalDev()`; without it, rayfin-local serves
    /// `/rayfin.config.json` and proxies the SDK's routes itself.
    pub fn uses_rayfin_plugin(&self) -> bool {
        self.rayfin_plugin
    }

    /// Waits until Vite exits by itself (it crashed or was killed), for supervision.
    pub async fn wait_exit(&mut self) -> Result<ExitStatus, FrontendError> {
        Ok(self.child.wait().await?)
    }

    /// Closes the server, then kills whatever is left of its process tree after 3 seconds.
    pub async fn stop(mut self) -> Result<(), FrontendError> {
        self.child.kill(STOP_GRACE).await?;
        Ok(())
    }
}

/// Passes Vite's output on to `sink`, except the announcement lines; until the first
/// announcement, the last lines are kept for an early-exit error.
fn announcing_sink(sink: LineSink, startup: Arc<Mutex<Startup>>) -> LineSink {
    Arc::new(move |label: &str, stream: Stream, line: &str| {
        let marked = if stream == Stream::Stdout {
            split_announcement(line)
        } else {
            None
        };
        let output = match &marked {
            Some((before, _)) => *before,
            None => line,
        };
        let forward = marked.is_none() || !output.is_empty();
        {
            let mut startup = startup.lock().unwrap_or_else(PoisonError::into_inner);
            if !output.is_empty() && startup.announce.is_some() {
                if startup.tail.len() == TAIL_LINES {
                    startup.tail.pop_front();
                }
                startup.tail.push_back(output.to_owned());
            }
            if let Some((_, announcement)) = marked
                && let Some(announce) = startup.announce.take()
            {
                let _ = announce.send(announcement);
            }
        }
        if forward {
            sink(label, stream, output);
        }
    })
}

/// Finds an announcement in a stdout line. Returns the output before the marker (a line Vite
/// left unterminated) and the parsed announcement.
fn split_announcement(line: &str) -> Option<(&str, Result<Announcement, String>)> {
    let at = line.find(MARKER)?;
    let rest = &line[at + MARKER.len()..];
    let (kind, json) = rest.split_once(' ').unwrap_or((rest, ""));
    let announcement = match kind {
        "vite-ready" => serde_json::from_str(json).map(Announcement::Ready),
        "vite-error" => serde_json::from_str(json).map(Announcement::Error),
        _ => return None,
    };
    Some((
        &line[..at],
        announcement.map_err(|e| format!("invalid `{kind}` announcement: {e}")),
    ))
}

fn start_error(failure: StartFailure, port: u16, frontend_dir: &Path) -> FrontendError {
    if let Some(stack) = &failure.stack {
        tracing::debug!("Vite failed to start: {stack}");
    }
    let code = failure.code.as_deref().unwrap_or_default();
    if code == "EADDRINUSE" || failure.message.contains("is already in use") {
        return FrontendError::PortInUse { port };
    }
    if code == "VITE_NOT_FOUND" {
        return FrontendError::ViteNotInstalled {
            dir: frontend_dir.to_owned(),
            hint: install_hint(frontend_dir),
        };
    }
    let hint = match code {
        "ERR_MODULE_NOT_FOUND" | "MODULE_NOT_FOUND" => format!(
            "\nA package is missing: run `npm install` in {}.",
            frontend_dir.display()
        ),
        "EACCES" if failure.message.contains("listen") => format!(
            "\nPort {port} may be reserved by the system: choose another one with \
             `--frontend-port`."
        ),
        _ => String::new(),
    };
    FrontendError::ViteFailed {
        message: failure.message,
        hint,
    }
}

/// Whether Node.js can resolve `vite` from the frontend dir (a hoisted workspace install counts).
fn vite_installed(frontend_dir: &Path) -> bool {
    frontend_dir.ancestors().any(|dir| {
        dir.join("node_modules")
            .join("vite")
            .join("package.json")
            .is_file()
    })
}

fn install_hint(frontend_dir: &Path) -> String {
    let declared = std::fs::read(frontend_dir.join("package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|manifest| {
            ["dependencies", "devDependencies"]
                .iter()
                .any(|section| manifest[section].get("vite").is_some())
        });
    if declared {
        format!("Run `npm install` in {}.", frontend_dir.display())
    } else {
        "rayfin-local runs the app's own Vite dev server. Install the app's packages \
         (`npm install`), or, if the app doesn't use Vite, serve its build with \
         `rayfin-local serve --static`."
            .to_owned()
    }
}

/// Whether Vite can listen on `port`. For `localhost` both loopback addresses must be free:
/// Vite binds just one of them, but a browser may connect to either.
fn port_is_free(host: &str, port: u16) -> bool {
    if port == 0 {
        return true;
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addresses: Vec<IpAddr> = if host.eq_ignore_ascii_case("localhost") {
        vec![Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()]
    } else if let Ok(address) = host.parse() {
        vec![address]
    } else {
        // Vite reports a conflict on other names itself.
        return true;
    };
    addresses
        .into_iter()
        .all(|address| match TcpListener::bind((address, port)) {
            Ok(_) => true,
            // Any other failure means the address doesn't exist here (IPv6 disabled).
            Err(error) => !matches!(
                error.kind(),
                io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
            ),
        })
}

fn fallback_url(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("http://[{host}]:{port}/")
    } else {
        format!("http://{host}:{port}/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announcements_are_split_from_output() {
        let (before, announcement) = split_announcement(
            r#"partial@@rayfin-local:vite-ready {"urls":["http://localhost:5175/"],"version":"7.3.5","rayfinPlugin":true}"#,
        )
        .unwrap();
        assert_eq!(before, "partial");
        let Ok(Announcement::Ready(ready)) = announcement else {
            panic!("{announcement:?}");
        };
        assert_eq!(ready.urls, ["http://localhost:5175/"]);
        assert!(ready.rayfin_plugin);

        let (_, error) =
            split_announcement(r#"@@rayfin-local:vite-error {"code":null,"message":"boom"}"#)
                .unwrap();
        assert!(matches!(error, Ok(Announcement::Error(f)) if f.message == "boom"));
        assert!(
            split_announcement("@@rayfin-local:vite-ready {")
                .unwrap()
                .1
                .is_err()
        );
        assert!(split_announcement("@@rayfin-local:ready {}").is_none());
        assert!(split_announcement("  ➜  Local:   http://localhost:5175/").is_none());
    }

    #[test]
    fn start_failures_get_actionable_errors() {
        let dir = Path::new("app");
        let failure = |code: Option<&str>, message: &str| StartFailure {
            code: code.map(str::to_owned),
            message: message.to_owned(),
            stack: None,
        };
        assert!(matches!(
            start_error(failure(None, "Port 5175 is already in use"), 5175, dir),
            FrontendError::PortInUse { port: 5175 }
        ));
        assert!(matches!(
            start_error(failure(Some("VITE_NOT_FOUND"), "no vite"), 5175, dir),
            FrontendError::ViteNotInstalled { .. }
        ));
        let missing = start_error(
            failure(
                Some("ERR_MODULE_NOT_FOUND"),
                "Cannot find package '@vitejs/plugin-react'",
            ),
            5175,
            dir,
        )
        .to_string();
        assert!(missing.contains("@vitejs/plugin-react"), "{missing}");
        assert!(missing.contains("npm install"), "{missing}");
        let reserved = start_error(
            failure(
                Some("EACCES"),
                "listen EACCES: permission denied 127.0.0.1:5175",
            ),
            5175,
            dir,
        )
        .to_string();
        assert!(reserved.contains("--frontend-port"), "{reserved}");
    }

    #[test]
    fn a_port_taken_on_either_loopback_is_not_free_for_localhost() {
        let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = taken.local_addr().unwrap().port();
        assert!(!port_is_free("localhost", port));
        assert!(!port_is_free("127.0.0.1", port));
        drop(taken);
        assert!(port_is_free("127.0.0.1", port));
        assert!(port_is_free("localhost", 0));
    }
}
