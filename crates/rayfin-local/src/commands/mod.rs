//! The subcommands, and the startup stages they share.

pub mod doctor;
pub mod reset;
pub mod schema;
pub mod serve;

use std::path::Path;

use rl_node::NodeRuntime;
use rl_project::Project;

use crate::ui;

/// Finds the project from `dir`, shows its warnings and checks its packages are installed.
pub fn load_project(dir: &Path) -> anyhow::Result<Project> {
    let project = Project::discover(dir)?;
    for warning in project.warnings() {
        ui::warn(warning);
    }
    project.check_installed()?;
    if let Some(warning) = project.sdk_version_warning() {
        ui::warn(warning);
    }
    ui::step(format_args!(
        "Project {} ({})",
        project.display_name(),
        project.root().display()
    ));
    Ok(project)
}

/// Finds Node.js (the project's own tools and the app's code run on it).
pub async fn find_node() -> anyhow::Result<NodeRuntime> {
    let node = NodeRuntime::discover().await?;
    tracing::debug!("Node.js {} at {}", node.version, node.path.display());
    Ok(node)
}
