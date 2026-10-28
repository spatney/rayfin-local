//! Finding and loading a Rayfin project.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::ProjectError;
use crate::config::{FunctionsAuthType, RayfinConfig, StaticHostingConfig};
use crate::env::{EnvFile, FRONTEND_PORT_VAR};
use crate::local_config::LocalConfig;
use crate::package::{
    CLIENT_PACKAGE, CORE_PACKAGE, PackageJson, RayfinPackage, collect_rayfin_packages,
    is_supported_sdk_version,
};
use crate::paths::{PathProblem, resolve_contained};
use crate::state::STATE_DIR;

/// Vite's default port, used when nothing else sets the frontend port.
pub const DEFAULT_FRONTEND_PORT: u16 = 5173;

/// How to load a project. The default reads `rayfin/.env` and the process environment.
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// An env file to use instead of `rayfin/.env` (relative to the project root or absolute);
    /// it must exist. Without it, `RAYFIN_ENV_FILE` from the environment is honored, as in the
    /// Rayfin CLI.
    pub env_file: Option<PathBuf>,
    /// The environment for `${VAR}` interpolation in `rayfin.yml` (it wins over the env file) and
    /// for `RAYFIN_ENV_FILE`. `None` uses the process environment.
    pub process_env: Option<BTreeMap<String, String>>,
}

/// A loaded Rayfin project: the directory that contains `rayfin/rayfin.yml`, its configuration,
/// `rayfin/.env`, `package.json` files and `rayfin-local.toml`.
///
/// Every path it returns is absolute and free of `\\?\` prefixes.
#[derive(Debug, Clone)]
pub struct Project {
    root: PathBuf,
    config: RayfinConfig,
    env: EnvFile,
    local_config: LocalConfig,
    root_package: Option<PackageJson>,
    frontend_package: Option<PackageJson>,
    rayfin_packages: Vec<RayfinPackage>,
    paths: ServicePaths,
    warnings: Vec<String>,
}

impl Project {
    /// Loads the project that contains `start`: the first of `start` and its parents with a
    /// `rayfin/rayfin.yml`.
    pub fn discover(start: &Path) -> Result<Self, ProjectError> {
        Self::discover_with(start, &LoadOptions::default())
    }

    /// [`Project::discover`] with options.
    pub fn discover_with(start: &Path, options: &LoadOptions) -> Result<Self, ProjectError> {
        Self::load_with(&Self::find_root(start)?, options)
    }

    /// The project root for `start`: the first of `start` and its parents that contains
    /// `rayfin/rayfin.yml`.
    pub fn find_root(start: &Path) -> Result<PathBuf, ProjectError> {
        let start = canonicalize(start)?;
        let dir = if start.is_file() {
            start.parent().unwrap_or(&start)
        } else {
            &start
        };
        dir.ancestors()
            .find(|dir| dir.join("rayfin").join("rayfin.yml").is_file())
            .map(Path::to_path_buf)
            .ok_or(ProjectError::NotFound {
                start: dir.to_path_buf(),
            })
    }

    /// Loads the project rooted at `root` (the directory that contains `rayfin/`).
    pub fn load(root: &Path) -> Result<Self, ProjectError> {
        Self::load_with(root, &LoadOptions::default())
    }

    /// [`Project::load`] with options.
    pub fn load_with(root: &Path, options: &LoadOptions) -> Result<Self, ProjectError> {
        let root = canonicalize(root)?;
        let config_path = root.join("rayfin").join("rayfin.yml");
        if !config_path.is_file() {
            return Err(ProjectError::NotFound { start: root });
        }
        let process_env = options.process_env.clone().unwrap_or_else(process_env);
        let env = EnvFile::load(&env_file_path(&root, options, &process_env)?)?;
        let mut vars = env.vars().clone();
        vars.extend(process_env);
        let config = RayfinConfig::load(&config_path, &vars, env.path())?;
        let local_config = LocalConfig::load(&root.join(LocalConfig::FILE_NAME))?;

        let mut warnings: Vec<String> = config
            .unknown_keys
            .iter()
            .map(|key| format!("rayfin/rayfin.yml: unknown setting `{key}` is ignored."))
            .collect();
        let paths = ServicePaths::resolve(&root, &config, &config_path, &mut warnings)?;
        warnings.extend(setting_warnings(&config, &env));

        let root_package = PackageJson::load_dir(&root)?;
        let frontend_package = if paths.frontend_dir == root {
            root_package.clone()
        } else {
            PackageJson::load_dir(&paths.frontend_dir)?
        };
        let manifests: Vec<&PackageJson> =
            frontend_package.iter().chain(root_package.iter()).collect();
        let rayfin_packages = collect_rayfin_packages(&paths.frontend_dir, &root, &manifests);

        Ok(Self {
            root,
            config,
            env,
            local_config,
            root_package,
            frontend_package,
            rayfin_packages,
            paths,
            warnings,
        })
    }

    /// The project root: the directory that contains `rayfin/rayfin.yml`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/rayfin`.
    pub fn rayfin_dir(&self) -> PathBuf {
        self.root.join("rayfin")
    }

    /// `<root>/rayfin/rayfin.yml`.
    pub fn config_path(&self) -> PathBuf {
        self.rayfin_dir().join("rayfin.yml")
    }

    /// The parsed `rayfin.yml`.
    pub fn config(&self) -> &RayfinConfig {
        &self.config
    }

    /// The env file (`rayfin/.env` unless overridden); empty when it does not exist.
    pub fn env(&self) -> &EnvFile {
        &self.env
    }

    /// `rayfin-local.toml` (the defaults when the file does not exist).
    pub fn local_config(&self) -> &LocalConfig {
        &self.local_config
    }

    /// `<root>/rayfin-local.toml`.
    pub fn local_config_path(&self) -> PathBuf {
        self.root.join(LocalConfig::FILE_NAME)
    }

    /// `<root>/.rayfin-local`, where everything rayfin-local writes goes.
    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }

    /// The name to show: `name`, else `id`, else the root folder's name.
    pub fn display_name(&self) -> &str {
        let config = &self.config;
        [config.name.as_deref(), config.id.as_deref()]
            .into_iter()
            .flatten()
            .find(|name| !name.trim().is_empty())
            .or_else(|| self.root.file_name().and_then(|name| name.to_str()))
            .unwrap_or("app")
    }

    /// Problems that do not stop rayfin-local, for the developer to see: unknown `rayfin.yml`
    /// settings, ignored values and configurations `rayfin up` would reject. The SDK version
    /// check is separate ([`Project::sdk_version_warning`]).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// The data package (`services.data.path`, default: the root). Its build command runs here.
    pub fn data_service_root(&self) -> &Path {
        &self.paths.data_service_root
    }

    /// The entity sources, `<data package>/rayfin/data`. A data package whose `package.json`
    /// has `exports` or `main` is loaded from its build output instead (the CLI does the same).
    pub fn data_dir(&self) -> PathBuf {
        self.paths.data_service_root.join("rayfin").join("data")
    }

    /// The storage package (`services.storage.path`, default: the root).
    pub fn storage_service_root(&self) -> &Path {
        &self.paths.storage_service_root
    }

    /// The storage (blob folder) sources, `<storage package>/rayfin/storage`.
    pub fn storage_schema_dir(&self) -> PathBuf {
        self.paths
            .storage_service_root
            .join("rayfin")
            .join("storage")
    }

    /// The functions package (`services.functions.path`, default `rayfin/functions`).
    pub fn functions_dir(&self) -> &Path {
        &self.paths.functions_dir
    }

    /// Where `staticHosting.buildCommand` runs: `services.staticHosting.path`, default the root.
    pub fn static_build_dir(&self) -> &Path {
        &self.paths.static_build_dir
    }

    /// The frontend project (where `vite.config.*` and its `package.json` are):
    /// `services.staticHosting.path` and then `root`, default the project root.
    pub fn frontend_dir(&self) -> &Path {
        &self.paths.frontend_dir
    }

    /// The built frontend: `staticHosting.folder` (default `dist`) in the frontend project.
    pub fn static_output_dir(&self) -> &Path {
        &self.paths.static_output_dir
    }

    /// `<root>/package.json`.
    pub fn root_package(&self) -> Option<&PackageJson> {
        self.root_package.as_ref()
    }

    /// The frontend project's `package.json` (the root's when the frontend is the root).
    pub fn frontend_package(&self) -> Option<&PackageJson> {
        self.frontend_package.as_ref()
    }

    /// Every `@microsoft/rayfin-*` package declared in those `package.json` files or installed,
    /// sorted by name.
    pub fn rayfin_packages(&self) -> &[RayfinPackage] {
        &self.rayfin_packages
    }

    /// One `@microsoft/rayfin-*` package, by full name or by `rayfin-…` alone.
    pub fn rayfin_package(&self, name: &str) -> Option<&RayfinPackage> {
        let name = name.strip_prefix("@microsoft/").unwrap_or(name);
        self.rayfin_packages
            .iter()
            .find(|p| p.name.strip_prefix("@microsoft/") == Some(name))
    }

    /// The Rayfin SDK version: `@microsoft/rayfin-client`'s, else `@microsoft/rayfin-core`'s
    /// (installed beats declared).
    pub fn sdk_version(&self) -> Option<&str> {
        self.sdk_package().and_then(RayfinPackage::version)
    }

    fn sdk_package(&self) -> Option<&RayfinPackage> {
        [CLIENT_PACKAGE, CORE_PACKAGE]
            .into_iter()
            .filter_map(|name| self.rayfin_package(name))
            .find(|package| package.version().is_some())
    }

    /// A warning when the SDK is not 1.36.x, the contract rayfin-local implements.
    pub fn sdk_version_warning(&self) -> Option<String> {
        let package = self.sdk_package()?;
        let version = package.version()?;
        (!is_supported_sdk_version(version)).then(|| {
            format!(
                "{} {version} is not the 1.36 SDK rayfin-local implements; some behavior may \
                 differ from Fabric. Use @microsoft/rayfin-* 1.36.x for full fidelity.",
                package.name
            )
        })
    }

    /// Fails with a hint to run `npm install` when `@microsoft/rayfin-core` is not installed
    /// (rayfin-local runs the project's own SDK and tools).
    pub fn check_installed(&self) -> Result<(), ProjectError> {
        if self
            .rayfin_package(CORE_PACKAGE)
            .is_some_and(|p| p.installed.is_some())
        {
            return Ok(());
        }
        let frontend_dir = &self.paths.frontend_dir;
        let install_dir = if self.root_package.is_none() && self.frontend_package.is_some() {
            frontend_dir.clone()
        } else {
            self.root.clone()
        };
        Err(ProjectError::NotInstalled {
            package: CORE_PACKAGE.into(),
            searched_from: frontend_dir.clone(),
            install_dir,
        })
    }

    /// The frontend port to prefer: `[frontend] port` in `rayfin-local.toml`, else
    /// `RAYFIN_PUBLIC_FRONTEND_PORT`, else 5173. (A `--frontend-port` flag beats all three.)
    pub fn preferred_frontend_port(&self) -> u16 {
        self.local_config
            .frontend
            .port
            .or_else(|| self.env.frontend_port())
            .unwrap_or(DEFAULT_FRONTEND_PORT)
    }
}

/// The env file to read: an explicit one (which must exist), else `RAYFIN_ENV_FILE`, else
/// `rayfin/.env` (the CLI's `resolveEnvFilePath`).
fn env_file_path(
    root: &Path,
    options: &LoadOptions,
    process_env: &BTreeMap<String, String>,
) -> Result<PathBuf, ProjectError> {
    if let Some(explicit) = &options.env_file {
        let path = root.join(explicit);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(ProjectError::EnvFileNotFound { path })
        };
    }
    Ok(match process_env.get("RAYFIN_ENV_FILE") {
        Some(path) if !path.is_empty() => root.join(path),
        _ => root.join("rayfin").join(".env"),
    })
}

/// Settings that work locally but that the developer should hear about.
fn setting_warnings(config: &RayfinConfig, env: &EnvFile) -> Vec<String> {
    let services = &config.services;
    let mut warnings = Vec::new();
    if let Some(port) = env.get(FRONTEND_PORT_VAR)
        && env.frontend_port().is_none()
    {
        warnings.push(format!(
            "rayfin/.env: {FRONTEND_PORT_VAR} '{port}' is not a valid port and is ignored."
        ));
    }
    let functions = &services.functions;
    if functions.enabled
        && functions.auth.and_then(|auth| auth.auth_type) != Some(FunctionsAuthType::Application)
    {
        warnings.push(
            "rayfin/rayfin.yml: services.functions.auth.type should be `application`; `rayfin \
             up` rejects other values when functions are enabled."
                .into(),
        );
    }
    if services.storage.enabled && !services.data.enabled {
        warnings.push(
            "rayfin/rayfin.yml: storage requires the data service. Enable services.data or \
             disable services.storage."
                .into(),
        );
    }
    warnings
}

/// The directories of the services, resolved from `rayfin.yml`.
#[derive(Debug, Clone)]
struct ServicePaths {
    data_service_root: PathBuf,
    storage_service_root: PathBuf,
    functions_dir: PathBuf,
    static_build_dir: PathBuf,
    frontend_dir: PathBuf,
    static_output_dir: PathBuf,
}

impl ServicePaths {
    fn resolve(
        root: &Path,
        config: &RayfinConfig,
        config_path: &Path,
        warnings: &mut Vec<String>,
    ) -> Result<Self, ProjectError> {
        let services = &config.services;
        let (data, storage, functions) = (&services.data, &services.storage, &services.functions);
        let hosting = &services.static_hosting;
        let mut paths = PathResolver {
            config_path,
            warnings,
        };

        let data_service_root = paths.resolve(
            Setting::path("data", data.path.as_deref(), data.enabled),
            root,
            root,
        )?;
        let storage_service_root = paths.resolve(
            Setting::path("storage", storage.path.as_deref(), storage.enabled),
            root,
            root,
        )?;
        let functions_dir = paths.resolve(
            Setting::path("functions", functions.path.as_deref(), functions.enabled),
            root,
            &root.join("rayfin").join("functions"),
        )?;
        let static_build_dir = paths.resolve(
            Setting::path("staticHosting", hosting.path.as_deref(), hosting.enabled),
            root,
            root,
        )?;
        let frontend_dir = paths.resolve(
            Setting {
                field: "root",
                ..Setting::path("staticHosting", hosting.root.as_deref(), hosting.enabled)
            },
            &static_build_dir,
            &static_build_dir,
        )?;
        let static_output_dir = paths.resolve(
            Setting {
                field: "folder",
                ..Setting::path("staticHosting", Some(hosting.folder()), hosting.enabled)
            },
            &frontend_dir,
            &frontend_dir.join(StaticHostingConfig::DEFAULT_FOLDER),
        )?;
        Ok(Self {
            data_service_root,
            storage_service_root,
            functions_dir,
            static_build_dir,
            frontend_dir,
            static_output_dir,
        })
    }
}

/// A path setting of a service in `rayfin.yml`.
struct Setting<'a> {
    service: &'static str,
    /// `path`, or `root`/`folder` for static hosting.
    field: &'static str,
    value: Option<&'a str>,
    enabled: bool,
}

impl<'a> Setting<'a> {
    fn path(service: &'static str, value: Option<&'a str>, enabled: bool) -> Self {
        Self {
            service,
            field: "path",
            value,
            enabled,
        }
    }

    /// What the value is relative to, for messages.
    fn base_name(&self) -> &'static str {
        match self.field {
            "root" => "services.staticHosting.path",
            "folder" => "the frontend directory",
            _ => "the project",
        }
    }
}

struct PathResolver<'a> {
    config_path: &'a Path,
    warnings: &'a mut Vec<String>,
}

impl PathResolver<'_> {
    /// Resolves a path setting against `base`. An invalid value fails for an enabled service;
    /// for a disabled one it is ignored with a warning, and `default` is used.
    fn resolve(
        &mut self,
        setting: Setting<'_>,
        base: &Path,
        default: &Path,
    ) -> Result<PathBuf, ProjectError> {
        let Some(value) = setting.value.filter(|value| !value.trim().is_empty()) else {
            return Ok(default.to_path_buf());
        };
        let reason = match resolve_contained(base, value) {
            Ok(path) => return Ok(path),
            Err(PathProblem::Absolute) => "it must be a relative path".to_string(),
            Err(PathProblem::Escapes) => format!("it points outside {}", setting.base_name()),
        };
        let Setting {
            service,
            field,
            enabled,
            ..
        } = setting;
        if enabled {
            return Err(ProjectError::InvalidServicePath {
                path: self.config_path.to_path_buf(),
                service,
                field,
                value: value.into(),
                reason,
            });
        }
        self.warnings.push(format!(
            "rayfin/rayfin.yml: services.{service}.{field} '{value}' is ignored because {reason}."
        ));
        Ok(default.to_path_buf())
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf, ProjectError> {
    dunce::canonicalize(path).map_err(|source| ProjectError::Read {
        path: path.to_path_buf(),
        source,
    })
}

fn process_env() -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}
