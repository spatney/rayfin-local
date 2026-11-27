//! The functions' generated TypeScript, kept current as `rayfin dev` keeps it: `src/types.ts`, whose
//! `AppFunctionsSchema` types `client.functions.<name>.invoke(...)` in the frontend, and
//! `src/secrets.generated.ts`, which types `ctx.Secrets`. [`FunctionsService`] generates them
//! when the functions start and again whenever their sources or secrets change, with the
//! project's own Rayfin CLI (`js/functions-types.mjs`): what they hold depends on its version.
//! The CLI leaves a file whose content stays the same as it is.
//!
//! [`FunctionsService`]: crate::FunctionsService

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use rl_node::{Helper, LineSink, NodeError, NodeRuntime};
use serde::Deserialize;

use crate::metadata::node_path;

const HELPER: Helper = Helper::new(
    "functions-types",
    include_str!("../../../js/functions-types.mjs"),
);

/// What a generation changed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Generated {
    /// The version of the Rayfin CLI that generated the files.
    pub cli_version: String,
    /// The files it changed, relative to the functions package, such as `src/types.ts`.
    pub written: Vec<String>,
}

/// The generated types could not be regenerated; the files keep what they had.
#[derive(Debug, thiserror::Error)]
pub enum TypegenError {
    #[error(
        "could not regenerate the functions' generated types (src/types.ts) in {}: {message}",
        dir.display()
    )]
    Generate { dir: PathBuf, message: String },
    #[error(
        "could not regenerate the functions' generated types (src/types.ts) in {}: {source}",
        dir.display()
    )]
    Node {
        dir: PathBuf,
        #[source]
        source: NodeError,
    },
}

/// How a generation went, for hosts ([`FunctionsConfig::on_typegen`]).
///
/// [`FunctionsConfig::on_typegen`]: crate::FunctionsConfig::on_typegen
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypegenOutcome {
    /// The generated files are current; `written` are those that changed.
    Current { written: Vec<String> },
    /// They could not be regenerated, for this reason.
    Failed { message: String },
}

/// Receives the outcome of each generation.
pub type TypegenObserver = Arc<dyn Fn(&TypegenOutcome) + Send + Sync>;

/// Regenerates the generated types of the functions in `functions_dir` with the Rayfin CLI that
/// Node resolves from there, as `rayfin dev` does. `cache_dir` holds the helper.
pub async fn generate(
    node: &NodeRuntime,
    functions_dir: &Path,
    cache_dir: &Path,
    sink: LineSink,
) -> Result<Generated, TypegenError> {
    run(node, functions_dir, cache_dir, sink, false).await
}

/// Regenerates only `src/secrets.generated.ts`, as `rayfin dev` does before it builds the
/// functions: a build of handlers that read `ctx.Secrets.<NAME>` fails without the names.
pub async fn generate_secrets(
    node: &NodeRuntime,
    functions_dir: &Path,
    cache_dir: &Path,
    sink: LineSink,
) -> Result<Generated, TypegenError> {
    run(node, functions_dir, cache_dir, sink, true).await
}

async fn run(
    node: &NodeRuntime,
    functions_dir: &Path,
    cache_dir: &Path,
    sink: LineSink,
    secrets_only: bool,
) -> Result<Generated, TypegenError> {
    let dir = node_path(functions_dir);
    let input = serde_json::json!({ "functionsDir": dir, "secretsOnly": secrets_only });
    rl_node::run_helper(node, &HELPER, cache_dir, &dir, &input, sink)
        .await
        .map_err(|error| match error {
            NodeError::Helper { message, .. } => TypegenError::Generate {
                dir: dir.clone(),
                message,
            },
            source => TypegenError::Node {
                dir: dir.clone(),
                source,
            },
        })
}
