//! `package.json` files and the installed `@microsoft/rayfin-*` packages.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::error::strip_position_suffix;
use crate::{ProjectError, resolve_node_package};

/// The SDK package the app talks to the backend through.
pub const CLIENT_PACKAGE: &str = "@microsoft/rayfin-client";
/// Entities, analysis and the shared runtime; every Rayfin project depends on it.
pub const CORE_PACKAGE: &str = "@microsoft/rayfin-core";
/// The SDK minor version rayfin-local implements (`major`, `minor`).
pub const SUPPORTED_SDK_VERSION: (u64, u64) = (1, 36);
/// The SDK version the conformance tests and the fixture app use.
pub const TESTED_SDK_VERSION: &str = "1.36.2";

const RAYFIN_PACKAGE_PREFIX: &str = "@microsoft/rayfin-";

/// The parts of a `package.json` rayfin-local uses.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageJson {
    path: PathBuf,
    pub name: Option<String>,
    pub version: Option<String>,
    pub scripts: BTreeMap<String, String>,
    pub dependencies: BTreeMap<String, String>,
    pub dev_dependencies: BTreeMap<String, String>,
    /// The whole document, for fields not modeled here (`main`, `exports`, `workspaces`, …).
    pub raw: serde_json::Value,
}

impl PackageJson {
    /// Reads `path`.
    pub fn load(path: &Path) -> Result<Self, ProjectError> {
        let content = std::fs::read_to_string(path).map_err(|source| ProjectError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(path, &content)
    }

    /// Reads `<dir>/package.json`, or `None` when there is none.
    pub fn load_dir(dir: &Path) -> Result<Option<Self>, ProjectError> {
        let path = dir.join("package.json");
        match std::fs::read_to_string(&path) {
            Ok(content) => Self::parse(&path, &content).map(Some),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ProjectError::Read { path, source }),
        }
    }

    /// Parses `content` (read from `path`). Entries that are not strings are skipped.
    pub fn parse(path: &Path, content: &str) -> Result<Self, ProjectError> {
        let content = content.strip_prefix('\u{feff}').unwrap_or(content);
        let raw: serde_json::Value = serde_json::from_str(content).map_err(|err| {
            let message = err.to_string();
            ProjectError::InvalidPackageJson {
                path: path.to_path_buf(),
                line: Some(err.line()),
                column: Some(err.column()),
                message: strip_position_suffix(&message, err.line(), err.column()).to_string(),
            }
        })?;
        if !raw.is_object() {
            return Err(ProjectError::InvalidPackageJson {
                path: path.to_path_buf(),
                line: None,
                column: None,
                message: "expected a JSON object".into(),
            });
        }
        let string = |key: &str| raw.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let strings = |key: &str| -> BTreeMap<String, String> {
            raw.get(key)
                .and_then(|v| v.as_object())
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        };
        Ok(Self {
            path: path.to_path_buf(),
            name: string("name"),
            version: string("version"),
            scripts: strings("scripts"),
            dependencies: strings("dependencies"),
            dev_dependencies: strings("devDependencies"),
            raw,
        })
    }

    /// The `package.json` file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The package directory.
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(&self.path)
    }

    pub fn script(&self, name: &str) -> Option<&str> {
        self.scripts.get(name).map(String::as_str)
    }

    /// The declared version range of `name` in `dependencies`, else `devDependencies`.
    pub fn dependency(&self, name: &str) -> Option<&str> {
        self.dependencies
            .get(name)
            .or_else(|| self.dev_dependencies.get(name))
            .map(String::as_str)
    }
}

/// An `@microsoft/rayfin-*` package the project declares or has installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RayfinPackage {
    /// For example `@microsoft/rayfin-client`.
    pub name: String,
    /// The version range in `package.json` (the frontend package's first, then the root's).
    pub declared: Option<String>,
    /// The version in `node_modules/<name>/package.json`, as Node resolves it from the frontend.
    pub installed: Option<String>,
    pub installed_dir: Option<PathBuf>,
}

impl RayfinPackage {
    /// The installed version, else the declared range.
    pub fn version(&self) -> Option<&str> {
        self.installed.as_deref().or(self.declared.as_deref())
    }
}

/// The `@microsoft/rayfin-*` packages declared in `manifests` (in priority order) or installed
/// in a `node_modules` between `from` and `root`, with versions resolved from `from`.
pub(crate) fn collect_rayfin_packages(
    from: &Path,
    root: &Path,
    manifests: &[&PackageJson],
) -> Vec<RayfinPackage> {
    let mut declared: BTreeMap<String, String> = BTreeMap::new();
    for manifest in manifests {
        for (name, range) in manifest
            .dependencies
            .iter()
            .chain(&manifest.dev_dependencies)
        {
            if name.starts_with(RAYFIN_PACKAGE_PREFIX) && !declared.contains_key(name) {
                declared.insert(name.clone(), range.clone());
            }
        }
    }
    let mut names: BTreeSet<String> = declared.keys().cloned().collect();
    for dir in from.ancestors().take_while(|dir| dir.starts_with(root)) {
        let Ok(entries) = std::fs::read_dir(dir.join("node_modules").join("@microsoft")) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && name.starts_with("rayfin-")
            {
                names.insert(format!("@microsoft/{name}"));
            }
        }
    }
    names
        .into_iter()
        .map(|name| {
            let installed_dir = resolve_node_package(from, &name);
            let installed = installed_dir.as_deref().and_then(installed_version);
            RayfinPackage {
                declared: declared.get(&name).cloned(),
                installed,
                installed_dir,
                name,
            }
        })
        .collect()
}

fn installed_version(dir: &Path) -> Option<String> {
    let manifest = PackageJson::load(&dir.join("package.json"))
        .inspect_err(|err| tracing::debug!("ignoring an unreadable installed package: {err}"))
        .ok()?;
    manifest.version
}

/// Whether a version (or a range such as `^1.36.2`) is in the SDK line rayfin-local implements.
pub fn is_supported_sdk_version(version: &str) -> bool {
    major_minor(version) == Some(SUPPORTED_SDK_VERSION)
}

/// The SDK line rayfin-local implements as a semver range, such as `>=1.36.0 <1.37.0`.
pub fn supported_sdk_range() -> String {
    let (major, minor) = SUPPORTED_SDK_VERSION;
    format!(">={major}.{minor}.0 <{major}.{}.0", minor + 1)
}

/// The SDK line rayfin-local implements, such as `1.36`.
pub fn supported_sdk_line() -> String {
    let (major, minor) = SUPPORTED_SDK_VERSION;
    format!("{major}.{minor}")
}

fn major_minor(version: &str) -> Option<(u64, u64)> {
    let version = version.trim_start_matches(|c: char| !c.is_ascii_digit());
    let mut parts = version.split('.').map(|part| {
        let digits = part.bytes().take_while(u8::is_ascii_digit).count();
        part[..digits].parse::<u64>().ok()
    });
    Some((parts.next()??, parts.next()??))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_fields_rayfin_local_uses() {
        let manifest = PackageJson::parse(
            Path::new("app/package.json"),
            "\u{feff}{\"name\":\"app\",\"scripts\":{\"dev\":\"vite\",\"bad\":1},\
             \"dependencies\":{\"@microsoft/rayfin-client\":\"1.36.2\"},\
             \"devDependencies\":{\"vite\":\"^7\"},\"main\":\"dist/index.js\"}",
        )
        .unwrap();
        assert_eq!(manifest.name.as_deref(), Some("app"));
        assert_eq!(manifest.scripts.len(), 1);
        assert_eq!(manifest.script("dev"), Some("vite"));
        assert_eq!(manifest.dependency("vite"), Some("^7"));
        assert_eq!(manifest.raw["main"], "dist/index.js");
    }

    #[test]
    fn reports_json_errors_with_position() {
        let err = PackageJson::parse(Path::new("package.json"), "{\n  \"name\": \n}").unwrap_err();
        assert!(
            matches!(
                err,
                ProjectError::InvalidPackageJson {
                    line: Some(3),
                    column: Some(1),
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn checks_the_sdk_line() {
        for version in ["1.36.2", "1.36.0-beta.1", "^1.36.2", "~1.36.0", ">=1.36.1"] {
            assert!(is_supported_sdk_version(version), "{version}");
        }
        for version in ["1.37.0", "1.3.6", "2.36.0", "latest", "workspace:*"] {
            assert!(!is_supported_sdk_version(version), "{version}");
        }
    }

    #[test]
    fn the_supported_range_covers_the_sdk_line_and_the_tested_version() {
        assert_eq!(supported_sdk_range(), ">=1.36.0 <1.37.0");
        assert!(is_supported_sdk_version(TESTED_SDK_VERSION));
    }
}
