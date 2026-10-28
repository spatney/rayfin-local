//! Building the functions package and watching it for changes.
//!
//! As `rayfin dev` does: one build with `services.functions.buildCommand` (default
//! `npm run build`) in the functions directory, then `npm run build:watch` when the package has
//! that script. rayfin-local also watches the compiled output, to restart the function host, and
//! the sources, to refresh `runtimemetadata.json`. A package without `build:watch` is rebuilt
//! with the build command when its sources change.

mod package;
mod watch;

use std::{
    fmt,
    path::{Component, Path, PathBuf},
    process::ExitStatus,
};

use rl_node::{LineSink, NodeError, ProcessSpec};

use self::package::Package;
pub use self::watch::{FunctionsWatch, WatchEvent, WatchMode, watch};

/// The build command when `rayfin.yml` sets no `services.functions.buildCommand`.
pub const DEFAULT_BUILD_COMMAND: &str = "npm run build";

/// The package script that recompiles continuously.
pub const BUILD_WATCH_SCRIPT: &str = "build:watch";

/// The log label of build output.
const LABEL: &str = "build";

/// How to build the functions package.
#[derive(Clone)]
pub struct BuildConfig {
    pub functions_dir: PathBuf,
    /// `services.functions.buildCommand`; [`DEFAULT_BUILD_COMMAND`] when unset or blank.
    pub build_command: Option<String>,
    /// Where build output goes, line by line, labelled `build`.
    pub sink: LineSink,
}

impl BuildConfig {
    /// The default build of `functions_dir`, with output to [`rl_node::default_sink`].
    pub fn new(functions_dir: impl Into<PathBuf>) -> Self {
        Self {
            functions_dir: functions_dir.into(),
            build_command: None,
            sink: rl_node::default_sink(),
        }
    }

    /// The command line that builds the package.
    pub fn command(&self) -> &str {
        self.build_command
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .unwrap_or(DEFAULT_BUILD_COMMAND)
    }
}

impl fmt::Debug for BuildConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildConfig")
            .field("functions_dir", &self.functions_dir)
            .field("build_command", &self.build_command)
            .finish_non_exhaustive()
    }
}

/// Building or watching the functions package failed.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(
        "functions build failed: `{command}` ended with {status} in {}. Fix the errors above or \
         run `{command}` in {}.",
        dir.display(),
        dir.display()
    )]
    Failed {
        command: String,
        dir: PathBuf,
        status: ExitStatus,
    },
    #[error("could not run the functions build `{command}` in {}: {source}", dir.display())]
    Run {
        command: String,
        dir: PathBuf,
        #[source]
        source: NodeError,
    },
    #[error(
        "could not start `npm run {BUILD_WATCH_SCRIPT}` in {}: {source}",
        dir.display()
    )]
    StartWatch {
        dir: PathBuf,
        #[source]
        source: NodeError,
    },
    #[error(
        "{} not found: the functions folder must be an npm package (check \
         services.functions.path in rayfin.yml)",
        path.display()
    )]
    MissingPackageJson { path: PathBuf },
    #[error("could not read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} is not valid JSON: {source}", path.display())]
    InvalidPackageJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "{} has no \"main\": set it to the compiled entry point of the functions, for example \
         \"dist/src/function_app.js\"",
        path.display()
    )]
    MissingMain { path: PathBuf },
    #[error(
        "\"main\" in {} is a glob (`{main}`): rayfin-local loads one entry file, so set \"main\" \
         to the file that registers the functions, for example \"dist/src/function_app.js\"",
        path.display()
    )]
    GlobMain { path: PathBuf, main: String },
    #[error(
        "the functions entry point {} does not exist: build the functions (`{DEFAULT_BUILD_COMMAND}` \
         in {}) or fix \"main\" in package.json",
        entry.display(),
        dir.display()
    )]
    NotBuilt { entry: PathBuf, dir: PathBuf },
    #[error("could not watch {}: {source}", path.display())]
    Watch {
        path: PathBuf,
        #[source]
        source: notify::Error,
    },
}

/// Runs the build command in the functions directory with the platform shell, streaming its
/// output to the config's sink.
pub async fn build(config: &BuildConfig) -> Result<(), BuildError> {
    let command = config.command();
    let dir = &config.functions_dir;
    tracing::info!("building the functions: `{command}` in {}", dir.display());
    let spec = ProcessSpec::shell(LABEL, command, dir).sink(config.sink.clone());
    rl_node::run_to_completion(spec)
        .await
        .map_err(|error| match error {
            NodeError::Failed { status, .. } => BuildError::Failed {
                command: command.to_owned(),
                dir: dir.clone(),
                status,
            },
            source => BuildError::Run {
                command: command.to_owned(),
                dir: dir.clone(),
                source,
            },
        })
}

/// The compiled entry point: `main` of the functions' `package.json`, which must exist.
pub fn entry_point(functions_dir: &Path) -> Result<PathBuf, BuildError> {
    let dir = normalize(functions_dir);
    let package = Package::read(&dir)?;
    let entry = normalize(&dir.join(package.main()?));
    if entry.is_file() {
        Ok(entry)
    } else {
        Err(BuildError::NotBuilt { entry, dir })
    }
}

/// The folders the build writes the code that runs to: the `outDir` of `tsconfig.json` when it
/// contains `main`, else (also) the folder of `main`.
pub fn output_dirs(functions_dir: &Path) -> Result<Vec<PathBuf>, BuildError> {
    let dir = normalize(functions_dir);
    let package = Package::read(&dir)?;
    output_dirs_of(&dir, &package)
}

fn output_dirs_of(dir: &Path, package: &Package) -> Result<Vec<PathBuf>, BuildError> {
    let entry = normalize(&dir.join(package.main()?));
    let main_dir = entry.parent().unwrap_or(dir).to_owned();
    let out_dir = package::tsconfig_out_dir(dir)
        .filter(|out_dir| out_dir.as_path() != dir && out_dir.starts_with(dir));
    Ok(match out_dir {
        Some(out_dir) if main_dir.starts_with(&out_dir) => vec![out_dir],
        Some(out_dir) => vec![out_dir, main_dir],
        None => vec![main_dir],
    })
}

/// Resolves `.` and `..` without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => normalized.push(".."),
            },
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc};

    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn package(dir: &Path, main: &str) {
        write(
            &dir.join("package.json"),
            &format!(r#"{{"name": "fns", "main": {}}}"#, serde_json::json!(main)),
        );
    }

    #[test]
    fn the_build_command_defaults_like_the_cli() {
        let mut config = BuildConfig {
            functions_dir: "fns".into(),
            build_command: None,
            sink: Arc::new(|_, _, _| {}),
        };
        assert_eq!(config.command(), "npm run build");
        config.build_command = Some("  ".into());
        assert_eq!(config.command(), "npm run build");
        config.build_command = Some("npm run compile && npm run bundle".into());
        assert_eq!(config.command(), "npm run compile && npm run bundle");
    }

    #[test]
    fn entry_point_is_main_once_built() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(matches!(
            entry_point(root),
            Err(BuildError::MissingPackageJson { .. })
        ));

        write(&root.join("package.json"), r#"{"name": "fns"}"#);
        let error = entry_point(root).unwrap_err();
        assert!(matches!(error, BuildError::MissingMain { .. }), "{error}");

        package(root, "dist/src/functions/*.js");
        assert!(matches!(
            entry_point(root),
            Err(BuildError::GlobMain { .. })
        ));

        package(root, "./dist/src/function_app.js");
        let error = entry_point(root).unwrap_err();
        assert!(matches!(error, BuildError::NotBuilt { .. }));
        assert!(error.to_string().contains("npm run build"), "{error}");

        write(&root.join("dist/src/function_app.js"), "");
        assert_eq!(
            entry_point(root).unwrap(),
            root.join("dist").join("src").join("function_app.js")
        );

        write(&root.join("package.json"), "{");
        assert!(matches!(
            entry_point(root),
            Err(BuildError::InvalidPackageJson { .. })
        ));
    }

    #[test]
    fn output_dirs_come_from_out_dir_or_main() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        package(root, "dist/src/function_app.js");
        assert_eq!(output_dirs(root).unwrap(), [root.join("dist").join("src")]);

        write(
            &root.join("tsconfig.json"),
            r#"{
                // Comments and trailing commas are allowed.
                "compilerOptions": { "outDir": "./dist", /* emitted code */ "rootDir": ".", },
                "include": ["src/**/*"],
            }"#,
        );
        assert_eq!(output_dirs(root).unwrap(), [root.join("dist")]);

        write(
            &root.join("tsconfig.json"),
            r#"{"extends": "./tsconfig.base", "include": ["src"]}"#,
        );
        write(
            &root.join("tsconfig.base.json"),
            r#"{"compilerOptions": {"outDir": "out"}}"#,
        );
        assert_eq!(
            output_dirs(root).unwrap(),
            [root.join("out"), root.join("dist").join("src")]
        );

        write(
            &root.join("tsconfig.json"),
            r#"{"compilerOptions": {"outDir": "."}}"#,
        );
        package(root, "index.js");
        assert_eq!(output_dirs(root).unwrap(), [root.to_path_buf()]);
    }

    #[test]
    fn normalize_is_lexical() {
        assert_eq!(normalize(Path::new("a/./b/../c")), Path::new("a/c"));
        assert_eq!(normalize(Path::new("../a/../../b")), Path::new("../../b"));
        assert_eq!(normalize(Path::new("/a/../..")), Path::new("/"));
    }
}
