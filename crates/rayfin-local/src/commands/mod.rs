//! The subcommands, and the startup stages they share.

pub mod doctor;
pub mod reset;
pub mod schema;
pub mod secret;
pub mod serve;
pub mod users;
pub mod version;

use std::path::Path;

use rl_node::NodeRuntime;
use rl_project::Project;
use serde_json::json;

use crate::events::WarningCode;
use crate::ui;

/// Finds the project from `dir`, shows its warnings and checks its packages are installed.
pub fn load_project(dir: &Path) -> anyhow::Result<Project> {
    let project = Project::discover(dir)?;
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

/// Finds Node.js (the project's own tools and the app's code run on it).
pub async fn find_node() -> anyhow::Result<NodeRuntime> {
    let node = NodeRuntime::discover().await?;
    tracing::debug!("Node.js {} at {}", node.version, node.path.display());
    Ok(node)
}
