//! The app's frontend: the project's own Vite dev server (default) or a static build
//! (`--static`), run with the environment that points the Rayfin SDK at the local backend.
//!
//! - [`frontend_env`] maps the local backend's settings to the variables the app, the SDK and
//!   `rayfinLocalDev()` (from `@microsoft/rayfin-local-dev/vite`) read. They are set on the
//!   process, where they beat the deployed values `rayfin env` wrote to `.env.local`; no file is
//!   written.
//! - [`DevServer`] runs the app's Vite through `js/vite-dev.mjs`, never `npm run dev` (which may
//!   call `rayfin up` or `rayfin dev`).
//! - [`build_static`] runs `services.staticHosting.buildCommand` and checks its output, for
//!   `rl_server::StaticSite`.

mod build;
mod dev;
mod env;

use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use rl_node::NodeError;

pub use build::{DEFAULT_BUILD_COMMAND, DEFAULT_INDEX_DOCUMENT, StaticBuildConfig, build_static};
pub use dev::{DevServer, DevServerConfig};
pub use env::{RayfinEnv, frontend_env};

/// Why the frontend could not start or build.
#[derive(Debug, thiserror::Error)]
pub enum FrontendError {
    #[error(
        "The frontend folder {} does not exist. Check `services.staticHosting.path` in \
         rayfin/rayfin.yml.",
        .0.display()
    )]
    MissingFrontendDir(PathBuf),
    #[error("Vite is not installed in {}. {hint}", dir.display())]
    ViteNotInstalled { dir: PathBuf, hint: String },
    #[error(
        "Port {port} is already in use, so the Vite dev server can't start on it. Stop the \
         program using it, or choose another port with `--frontend-port`."
    )]
    PortInUse { port: u16 },
    #[error("Vite failed to start: {message}{hint}")]
    ViteFailed { message: String, hint: String },
    #[error(
        "Vite exited ({status}) before it was ready.{output}\nCheck the app's Vite config, and run \
         `npm install` in {} if packages are missing.",
        dir.display()
    )]
    ExitedBeforeReady {
        status: String,
        /// The last lines Vite printed, each on its own line.
        output: String,
        dir: PathBuf,
    },
    #[error(
        "Vite did not start within {} seconds. Check its output above; if it is still \
         optimizing dependencies, start `rayfin-local serve` again.",
        .0.as_secs()
    )]
    StartTimeout(Duration),
    #[error("Vite stopped unexpectedly: {0}")]
    Protocol(String),
    #[error(
        "The build command `{command}` failed ({status}). Fix the errors above (run `npm install` \
         first if packages are missing), then start `rayfin-local serve --static` again."
    )]
    BuildFailed { command: String, status: ExitStatus },
    #[error(
        "The build command `{command}` finished but did not produce {}. Check \
         `services.staticHosting.folder` and `indexDocument` in rayfin/rayfin.yml.",
        index.display()
    )]
    MissingBuildOutput { command: String, index: PathBuf },
    #[error(transparent)]
    Node(#[from] NodeError),
}

/// Whether children should print colors: they write to pipes, so they don't detect our terminal.
fn color_wanted() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}
