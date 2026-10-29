//! Rayfin project discovery and configuration: finding the project root (`rayfin/rayfin.yml`),
//! parsing `rayfin.yml`, `rayfin/.env` and `package.json`, optional `rayfin-local.toml` overrides,
//! and the `.rayfin-local/` state folder.
//!
//! ```no_run
//! use rl_project::{LocalState, Project};
//!
//! # fn main() -> Result<(), rl_project::ProjectError> {
//! let project = Project::discover(std::path::Path::new("."))?;
//! for warning in project.warnings().iter().chain(project.sdk_version_warning().as_ref()) {
//!     eprintln!("warning: {warning}");
//! }
//! project.check_installed()?;
//! let auth = &project.config().services.auth;
//! println!("{}: tokens last {} min", project.display_name(), auth.access_token_lifetime_minutes());
//!
//! let mut state = LocalState::load_or_create(&project)?;
//! state.data_mut().api_port = Some(7171);
//! state.save()?;
//! # Ok(())
//! # }
//! ```

pub mod config;
mod env;
mod error;
mod interpolate;
mod local_config;
mod package;
mod paths;
mod project;
mod state;

pub use config::RayfinConfig;
pub use env::{
    API_URL_VAR, EnvFile, FRONTEND_PORT_VAR, FUNCTIONS_URL_VAR, FrontendFramework, ITEM_ID_VAR,
    PORTAL_URL_VAR, PUBLISHABLE_KEY_VAR, RAYFIN_PUBLIC_PREFIX, TENANT_ID_VAR, WORKSPACE_ID_VAR,
    map_public_env, map_public_env_key, parse_env,
};
pub use error::ProjectError;
pub use local_config::{
    LocalAuthConfig, LocalConfig, LocalConnectorsConfig, LocalFrontendConfig, LocalFunctionsConfig,
    LocalServerConfig, LocalTokensConfig, TokenSourceKind,
};
pub use package::{
    CLIENT_PACKAGE, CORE_PACKAGE, PackageJson, RayfinPackage, SUPPORTED_SDK_VERSION,
    is_supported_sdk_version,
};
pub use paths::resolve_node_package;
pub use project::{DEFAULT_FRONTEND_PORT, LoadOptions, Project};
pub use state::{
    LocalState, PUBLISHABLE_KEY_PREFIX, STATE_DIR, ServeInfo, ServeLock, StateData, write_atomic,
};
