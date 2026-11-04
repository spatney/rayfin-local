//! The subcommands, and the startup stages they share.

pub mod backup;
pub mod doctor;
pub mod reset;
pub mod restore;
pub mod schema;
pub mod secret;
pub mod serve;
pub mod users;
pub mod version;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use rl_node::NodeRuntime;
use rl_project::{LoadOptions, Project, ProjectError};
use serde_json::json;

use crate::errors::HostError;
use crate::events::WarningCode;
use crate::snapshot::SnapshotError;
use crate::ui;

/// Where a command works: the directory to find the project from (`-C`), and the state folder
/// (`--state-dir`, `RAYFIN_LOCAL_STATE_DIR`) when it isn't the project's `.rayfin-local`.
#[derive(Debug, Clone)]
pub struct Location {
    pub dir: PathBuf,
    pub state_dir: Option<PathBuf>,
}

impl Location {
    /// The project that contains [`Location::dir`], with its state folder.
    pub fn discover(&self) -> Result<Project, ProjectError> {
        Project::discover_with(
            &self.dir,
            &LoadOptions {
                state_dir: self.state_dir.clone(),
                ..LoadOptions::default()
            },
        )
    }

    /// The state folder, without loading the project (whose settings may be broken): the
    /// project's, as [`Project::state_dir`]; with `--state-dir`, that folder even when no project
    /// is found.
    pub fn resolve_state_dir(&self) -> Result<PathBuf, ProjectError> {
        match (Project::find_root(&self.dir), &self.state_dir) {
            (Ok(root), state_dir) => Project::resolve_state_dir(&root, state_dir.as_deref()),
            (Err(_), Some(state_dir)) => Ok(state_dir.clone()),
            (Err(err), None) => Err(err),
        }
    }
}

impl From<&Path> for Location {
    /// The project around `dir`, with its default state folder.
    fn from(dir: &Path) -> Self {
        Self {
            dir: dir.to_owned(),
            state_dir: None,
        }
    }
}

/// Finds the project, shows its warnings and checks its packages are installed.
pub fn load_project(location: &Location) -> anyhow::Result<Project> {
    let project = location.discover()?;
    check_project(&project)?;
    Ok(project)
}

/// Shows the project's warnings and checks its packages are installed.
pub fn check_project(project: &Project) -> anyhow::Result<()> {
    for warning in project.warnings() {
        ui::warn(warning);
    }
    project.check_installed()?;
    if let Some(warning) = project.sdk_version_warning() {
        let details = json!({
            "version": project.sdk_version(),
            "supported": rl_project::supported_sdk_range(),
        });
        ui::warning(WarningCode::SdkUnsupported, warning, details);
    }
    ui::step(format_args!(
        "Project {} ({})",
        project.display_name(),
        project.root().display()
    ));
    Ok(())
}

/// A snapshot command's failure as an error whose chain the host error codes classify.
pub fn snapshot_error(err: SnapshotError) -> anyhow::Error {
    match err {
        SnapshotError::Project(err) => err.into(),
        err => err.into(),
    }
}

/// Prints a `--json` command's failure as one line on stdout:
/// `{"type":"error","code":"…","message":"…"}`. The code is a [`SnapshotError`]'s, else the host
/// contract's ([`HostError`]).
pub fn print_json_error(err: &anyhow::Error) {
    let code = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<SnapshotError>()?.code())
        .map(serde_json::Value::from)
        .unwrap_or_else(|| json!(HostError::classify(err).code));
    let line = json!({
        "type": "error",
        "code": code,
        "message": ui::error_lines(err).join("\n"),
    });
    let _ = writeln!(std::io::stdout(), "{line}");
}

/// Finds Node.js (the project's own tools and the app's code run on it).
pub async fn find_node() -> anyhow::Result<NodeRuntime> {
    let node = NodeRuntime::discover().await?;
    tracing::debug!("Node.js {} at {}", node.version, node.path.display());
    Ok(node)
}
