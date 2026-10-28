//! The app model rayfin-local serves, analysed from the project's `rayfin/data` entities by the
//! project's own `@microsoft/rayfin-core` (`js/analyze.mjs`), and the [`Caller`] every backend
//! route acts for.
//!
//! Getting a model:
//!
//! 1. Build an [`AnalyzeInput`] (usually [`AnalyzeInput::for_project`]).
//! 2. Run [`ANALYZE_JS`] with it through `rl_node::run_helper` (the binary wraps it in an
//!    `rl_node::Helper`; this crate does not depend on `rl-node`).
//! 3. Turn the helper's result into an [`AppModel`] with [`AppModel::from_analysis`].

pub mod caller;
mod convert;
mod model;
mod policy;

use std::path::{Path, PathBuf};

use serde::Serialize;

pub use caller::{Caller, CallerError, CallerResolver, Session, bearer_token};
pub use model::*;
pub use policy::{CompareOp, Literal, Operand, PolicyAstError, PolicyExpr};

/// The schema-analysis helper (`js/analyze.mjs`), run with the project's Node.js and packages.
pub const ANALYZE_JS: &str = include_str!("../../../js/analyze.mjs");

/// The input of [`ANALYZE_JS`] (serialized in camelCase).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzeInput {
    /// The project directory; the helper also runs there.
    pub project_root: PathBuf,
    /// The entity sources, normally `<project>/rayfin/data`.
    pub data_dir: PathBuf,
    /// The `@blob` sources; the helper defaults to `<data_dir>/../storage`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_dir: Option<PathBuf>,
    /// `rayfin/tsconfig.json`; `None` compiles with built-in defaults.
    pub tsconfig_path: Option<PathBuf>,
    /// Where the compiled JavaScript goes, emptied first. Inside the project it must be under
    /// `.rayfin-local/` (the helper refuses anything else).
    pub out_dir: PathBuf,
    /// `services.data.dialect`.
    pub dialect: Dialect,
    /// Whether to analyze the `@blob` storage folders too (`services.storage.enabled`).
    pub storage: bool,
    /// Echo the analyzer's log to the helper's stderr.
    pub verbose: bool,
}

impl AnalyzeInput {
    /// The usual layout: entities in `rayfin/data`, `rayfin/tsconfig.json` when it exists, and the
    /// compiled output in `.rayfin-local/cache/compiled`.
    pub fn for_project(project_root: &Path, dialect: Dialect, storage: bool) -> Self {
        let tsconfig = project_root.join("rayfin").join("tsconfig.json");
        Self {
            project_root: project_root.to_path_buf(),
            data_dir: project_root.join("rayfin").join("data"),
            storage_dir: None,
            tsconfig_path: tsconfig.is_file().then_some(tsconfig),
            out_dir: project_root
                .join(".rayfin-local")
                .join("cache")
                .join("compiled"),
            dialect,
            storage,
            verbose: false,
        }
    }

    /// The JSON document the helper reads from its `--input` file.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("AnalyzeInput serializes to JSON")
    }
}
