//! `--static`: the app's own build, run with the local Rayfin env.

use std::path::PathBuf;

use rl_node::{LineSink, NodeError, ProcessSpec, run_to_completion};

use crate::FrontendError;

/// What runs when `services.staticHosting.buildCommand` is not set.
pub const DEFAULT_BUILD_COMMAND: &str = "npm run build";
/// The SPA entry point when `services.staticHosting.indexDocument` is not set.
pub const DEFAULT_INDEX_DOCUMENT: &str = "index.html";
const LABEL: &str = "build";

/// How to build the app for `--static`.
pub struct StaticBuildConfig {
    /// Where the command runs: the folder with the app's `package.json`.
    pub frontend_dir: PathBuf,
    /// `services.staticHosting.buildCommand`; [`DEFAULT_BUILD_COMMAND`] when unset or blank.
    pub build_command: Option<String>,
    /// The build output (`services.staticHosting.folder`, usually `dist`); a relative path is
    /// resolved against `frontend_dir`.
    pub output_dir: PathBuf,
    /// `services.staticHosting.indexDocument`, relative to `output_dir`;
    /// [`DEFAULT_INDEX_DOCUMENT`] when unset or blank.
    pub index_document: Option<String>,
    /// The Rayfin env from [`crate::frontend_env`].
    pub env: Vec<(String, String)>,
    /// Receives the command's output, labeled `build`.
    pub sink: LineSink,
}

/// Runs the build command in the platform shell with `env` set, and returns the output folder
/// once it holds the index document.
///
/// npm also runs the script's `pre` hook: in Rayfin templates `prebuild` is
/// `rayfin env --framework vite`, which rewrites `.env.local` from `rayfin/.env` with the deployed
/// values. That is the app's own script, and the variables set here still win in Vite.
pub async fn build_static(config: StaticBuildConfig) -> Result<PathBuf, FrontendError> {
    let frontend_dir = dunce::simplified(&config.frontend_dir).to_path_buf();
    if !frontend_dir.is_dir() {
        return Err(FrontendError::MissingFrontendDir(frontend_dir));
    }
    let command = non_blank(config.build_command.as_deref())
        .unwrap_or(DEFAULT_BUILD_COMMAND)
        .to_owned();
    let mut spec = ProcessSpec::shell(LABEL, &command, &frontend_dir).sink(config.sink);
    if crate::color_wanted() {
        spec = spec.env("FORCE_COLOR", "1");
    }
    for (name, value) in &config.env {
        spec = spec.env(name, value);
    }

    tracing::debug!(%command, dir = %frontend_dir.display(), "building the app");
    match run_to_completion(spec).await {
        Ok(()) => {}
        Err(NodeError::Failed { status, .. }) => {
            return Err(FrontendError::BuildFailed { command, status });
        }
        Err(error) => return Err(error.into()),
    }

    let output_dir = frontend_dir.join(&config.output_dir);
    let index_document = non_blank(config.index_document.as_deref())
        .unwrap_or(DEFAULT_INDEX_DOCUMENT)
        .trim_start_matches(['/', '\\']);
    let index = output_dir.join(index_document);
    if !index.is_file() {
        return Err(FrontendError::MissingBuildOutput { command, index });
    }
    Ok(output_dir)
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}
