//! `runtimemetadata.json` for the functions package.
//!
//! `@microsoft/fabric-user-data-functions` 1.36 never analyses source at runtime: `new
//! UserDataFunctions()` reads `runtimemetadata.json` from the process's working directory (beside
//! its default `srcRoot`, `./src`) and answers every invocation with "Unable to determine function
//! metadata" when the file is missing, invalid or not schema 2.x. The function host runs in the
//! functions directory, so the file belongs at `<functions dir>/runtimemetadata.json`, which is
//! where `rayfin dev` writes it too. It is a generated artifact, rewritten only when its content
//! changes; a stamp in the cache folder remembers that an older file was found current.
//!
//! `js/udf-metadata.mjs` generates it with the functions package's own TypeScript compiler,
//! following the rules of the Rayfin CLI 1.36.2 type generation, so the output is byte-for-byte
//! what `rayfin dev` writes for the same source.

mod model;

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub use model::{FunctionMetadata, ParameterMetadata, RuntimeMetadata};
use rl_node::{Helper, LineSink, NodeError, NodeRuntime};
use serde::{Deserialize, Serialize};

/// The file the UDF package reads, in the functions directory.
pub const FILE_NAME: &str = "runtimemetadata.json";

/// The schema version generated: the one the 1.36 UDF package requires.
pub const SCHEMA_VERSION: &str = "2.0";

const HELPER: Helper = Helper::new("udf-metadata", include_str!("../../../js/udf-metadata.mjs"));

/// The stamp of the last generation, in the cache folder.
const STAMP_FILE: &str = "runtimemetadata.stamp.json";

/// Runtime metadata could not be read or generated.
#[derive(Debug, thiserror::Error)]
pub enum MetadataError {
    #[error("could not read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "{} cannot be used by the functions runtime: {message}. It is regenerated from the \
         functions source when it is older than the source; delete it to force that.",
        path.display()
    )]
    Invalid { path: PathBuf, message: String },
    #[error("could not generate {FILE_NAME} for the functions in {}: {message}", dir.display())]
    Generate { dir: PathBuf, message: String },
    #[error("could not generate {FILE_NAME} for the functions in {}: {source}", dir.display())]
    Node {
        dir: PathBuf,
        #[source]
        source: NodeError,
    },
    #[error("could not write {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Where the UDF package reads the metadata of the functions in `functions_dir`.
pub fn metadata_path(functions_dir: &Path) -> PathBuf {
    functions_dir.join(FILE_NAME)
}

/// Reads and validates a metadata file.
pub fn load(path: &Path) -> Result<RuntimeMetadata, MetadataError> {
    let text = fs::read_to_string(path).map_err(|source| MetadataError::Read {
        path: path.to_owned(),
        source,
    })?;
    let invalid = |message: String| MetadataError::Invalid {
        path: path.to_owned(),
        message,
    };
    let metadata: RuntimeMetadata =
        serde_json::from_str(&text).map_err(|error| invalid(error.to_string()))?;
    metadata.validate().map_err(invalid)?;
    Ok(metadata)
}

/// Whether the metadata at `metadata_path` must be regenerated: it is missing, or older than one
/// of its inputs (`package.json`, `tsconfig*.json`, and the TypeScript files and folders under
/// `src/`, without `node_modules` and `dist`). Folders count so that removing a file counts.
pub fn is_stale(functions_dir: &Path, metadata_path: &Path) -> bool {
    is_older_than(metadata_path, newest_input(functions_dir))
}

fn is_older_than(metadata_path: &Path, newest_input: Option<SystemTime>) -> bool {
    let Ok(generated) = fs::metadata(metadata_path).and_then(|meta| meta.modified()) else {
        return true;
    };
    newest_input.is_some_and(|newest| newest > generated)
}

/// The input of one `js/udf-metadata.mjs` run.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateInput {
    /// The functions package: the folder with `package.json`, `tsconfig.json` and `src/`.
    pub functions_dir: PathBuf,
    /// More folders to resolve `typescript` from when neither the functions package nor its
    /// ancestors have it (tried before the Rayfin CLI's own copy).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resolve_paths: Vec<PathBuf>,
    /// A file for the helper to write (only if its content changes); `None` to only return it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_path: Option<PathBuf>,
}

impl GenerateInput {
    pub fn new(functions_dir: impl Into<PathBuf>) -> Self {
        Self {
            functions_dir: functions_dir.into(),
            resolve_paths: Vec::new(),
            output_path: None,
        }
    }
}

/// What `js/udf-metadata.mjs` produced.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Generated {
    /// The file content, exactly as the Rayfin CLI writes it.
    pub json: String,
    pub metadata: RuntimeMetadata,
    /// Whether [`GenerateInput::output_path`] was written; `None` without one.
    pub written: Option<bool>,
    pub diagnostics: Vec<Diagnostic>,
    pub typescript: TypeScriptInfo,
    /// Whether the audiences were resolved by the type checker (the package has a usable
    /// `tsconfig.json`) rather than read from the annotation text.
    pub type_checked: bool,
    /// The analysed files, relative to the functions directory, in output order.
    pub source_files: Vec<String>,
}

/// A warning about one function's metadata.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    pub file_path: String,
    pub line: u32,
    pub column: u32,
    pub function_name: String,
}

/// The TypeScript compiler the metadata was generated with.
#[derive(Debug, Clone, Deserialize)]
pub struct TypeScriptInfo {
    pub version: String,
    pub path: PathBuf,
}

/// Generates the metadata with `js/udf-metadata.mjs`, run in the functions directory. Its
/// diagnostics are logged as warnings.
pub async fn generate(
    node: &NodeRuntime,
    input: &GenerateInput,
    cache_dir: &Path,
    sink: LineSink,
) -> Result<Generated, MetadataError> {
    let dir = node_path(&input.functions_dir);
    let generate_error = |message: String| MetadataError::Generate {
        dir: dir.clone(),
        message,
    };
    if !dir.is_dir() {
        return Err(generate_error(
            "the folder does not exist (check services.functions.path in rayfin.yml)".into(),
        ));
    }
    let request = GenerateInput {
        functions_dir: dir.clone(),
        resolve_paths: input.resolve_paths.iter().map(|p| node_path(p)).collect(),
        output_path: input.output_path.as_deref().map(node_path),
    };
    let request =
        serde_json::to_value(&request).map_err(|error| generate_error(error.to_string()))?;
    let generated: Generated = rl_node::run_helper(node, &HELPER, cache_dir, &dir, &request, sink)
        .await
        .map_err(|error| match error {
            NodeError::Helper { message, .. } => generate_error(message),
            source => MetadataError::Node {
                dir: dir.clone(),
                source,
            },
        })?;
    for diagnostic in &generated.diagnostics {
        tracing::warn!(
            "{}:{}:{}: {}",
            diagnostic.file_path,
            diagnostic.line,
            diagnostic.column,
            diagnostic.message
        );
    }
    Ok(generated)
}

/// Returns the functions' metadata, regenerating `<functions_dir>/runtimemetadata.json` first
/// when it is stale ([`is_stale`]) or unusable. The file is replaced atomically, and only when
/// its content changes. `cache_dir` holds the helper and a stamp that remembers when an older
/// file was found current, so it is not regenerated again until an input changes.
pub async fn ensure_fresh(
    node: &NodeRuntime,
    functions_dir: &Path,
    cache_dir: &Path,
    sink: LineSink,
) -> Result<RuntimeMetadata, MetadataError> {
    ensure_fresh_with(node, &GenerateInput::new(functions_dir), cache_dir, sink).await
}

/// [`ensure_fresh`] with generation options. The metadata is kept at
/// [`GenerateInput::output_path`] when set, else at `<functions_dir>/runtimemetadata.json`.
pub async fn ensure_fresh_with(
    node: &NodeRuntime,
    input: &GenerateInput,
    cache_dir: &Path,
    sink: LineSink,
) -> Result<RuntimeMetadata, MetadataError> {
    let functions_dir = &input.functions_dir;
    let path = input
        .output_path
        .clone()
        .unwrap_or_else(|| metadata_path(functions_dir));
    let newest = newest_input(functions_dir);
    if !is_older_than(&path, newest) || Stamp::vouches(cache_dir, &path, newest) {
        match load(&path) {
            Ok(metadata) => return Ok(metadata),
            Err(error) => tracing::info!("{error}"),
        }
    }
    let request = GenerateInput {
        output_path: None,
        ..input.clone()
    };
    let generated = generate(node, &request, cache_dir, sink).await?;
    generated
        .metadata
        .validate()
        .map_err(|message| MetadataError::Generate {
            dir: functions_dir.clone(),
            message: format!(
                "the functions runtime would reject it ({message}); fix the udf.func(...) \
                 registrations in src/"
            ),
        })?;
    let written =
        write_if_changed(&path, &generated.json).map_err(|source| MetadataError::Write {
            path: path.clone(),
            source,
        })?;
    if written {
        tracing::info!("updated {}", path.display());
    }
    Stamp::save(cache_dir, &path, newest, &generated.json);
    Ok(generated.metadata)
}

/// What the last generation produced: a metadata file with this content is current while no
/// input is newer than `inputs_modified`, even when the file itself is older (it is not rewritten
/// when its content stays the same).
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stamp {
    metadata_path: PathBuf,
    /// The newest input before generation, as seconds and nanoseconds since the Unix epoch.
    inputs_modified: Option<(u64, u32)>,
    content: String,
}

impl Stamp {
    fn vouches(cache_dir: &Path, path: &Path, newest_input: Option<SystemTime>) -> bool {
        let Some(stamp) = fs::read(cache_dir.join(STAMP_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Stamp>(&bytes).ok())
        else {
            return false;
        };
        stamp.metadata_path == path
            && stamp.inputs_modified >= newest_input.map(epoch_time)
            && fs::read_to_string(path).is_ok_and(|content| content == stamp.content)
    }

    fn save(cache_dir: &Path, path: &Path, newest_input: Option<SystemTime>, content: &str) {
        let stamp = Stamp {
            metadata_path: path.to_owned(),
            inputs_modified: newest_input.map(epoch_time),
            content: content.to_owned(),
        };
        let saved = serde_json::to_vec(&stamp)
            .map_err(io::Error::other)
            .and_then(|bytes| {
                fs::create_dir_all(cache_dir)?;
                fs::write(cache_dir.join(STAMP_FILE), bytes)
            });
        if let Err(error) = saved {
            tracing::debug!("could not save the runtime metadata stamp: {error}");
        }
    }
}

fn epoch_time(time: SystemTime) -> (u64, u32) {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    (since.as_secs(), since.subsec_nanos())
}

/// Whether a file is a TypeScript source (`.ts`, `.mts`, `.cts`, `.tsx`).
pub(crate) fn is_typescript(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension, "ts" | "mts" | "cts" | "tsx"))
}

/// Whether a file directly in the functions directory is an input of the metadata.
pub(crate) fn is_package_config(file_name: &str) -> bool {
    file_name == "package.json"
        || (file_name.starts_with("tsconfig") && file_name.ends_with(".json"))
}

/// Whether a folder under `src/` is skipped when looking for sources.
pub(crate) fn is_ignored_source_dir(name: &str) -> bool {
    name == "node_modules" || name == "dist"
}

fn newest_input(functions_dir: &Path) -> Option<SystemTime> {
    let mut newest = None;
    let mut consider = |path: &Path| {
        if let Ok(modified) = fs::metadata(path).and_then(|meta| meta.modified()) {
            newest = newest.max(Some(modified));
        }
    };
    if let Ok(entries) = fs::read_dir(functions_dir) {
        for entry in entries.flatten() {
            if is_package_config(&entry.file_name().to_string_lossy()) {
                consider(&entry.path());
            }
        }
    }
    let src = functions_dir.join("src");
    if src.is_dir() {
        consider(&src);
        visit_sources(&src, &mut consider);
    }
    newest
}

fn visit_sources(dir: &Path, consider: &mut impl FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            if !is_ignored_source_dir(&entry.file_name().to_string_lossy()) {
                consider(&path);
                visit_sources(&path, consider);
            }
        } else if is_typescript(&path) {
            consider(&path);
        }
    }
}

/// Atomically replaces `path` with `contents` unless it already has them. Returns whether the
/// file was written.
fn write_if_changed(path: &Path, contents: &str) -> io::Result<bool> {
    match fs::read(path) {
        Ok(existing) if existing == contents.as_bytes() => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = path.with_file_name(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&temp, contents)?;
    if let Err(error) = replace(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(true)
}

/// Renames `from` over `to`, retrying briefly: Windows refuses while another process (an editor,
/// a virus scanner) has `to` open without delete sharing.
fn replace(from: &Path, to: &Path) -> io::Result<()> {
    let mut attempts = 0;
    loop {
        match fs::rename(from, to) {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied && attempts < 10 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

/// An absolute path as Node and TypeScript expect it (no `\\?\` prefix on Windows).
fn node_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    if cfg!(windows)
        && let Some(stripped) = absolute
            .to_str()
            .and_then(|text| text.strip_prefix(r"\\?\"))
            .filter(|text| text.as_bytes().get(1) == Some(&b':'))
    {
        return PathBuf::from(stripped);
    }
    absolute
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn modified(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    fn set_modified(path: &Path, time: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    /// Longer than the timestamp granularity of the file systems CI runs on.
    fn tick() {
        std::thread::sleep(Duration::from_millis(50));
    }

    #[test]
    fn staleness_follows_the_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for file in [
            "package.json",
            "tsconfig.json",
            "tsconfig.build.json",
            "local.settings.json",
            "src/function_app.ts",
            "src/nested/extra.mts",
            "src/readme.md",
            "src/node_modules/pkg/index.ts",
            "src/dist/out.ts",
            "dist/src/function_app.js",
        ] {
            write(&root.join(file), "x");
        }
        let metadata = metadata_path(root);
        assert!(is_stale(root, &metadata), "missing metadata is stale");

        tick();
        write(&metadata, "{}");
        assert!(!is_stale(root, &metadata));

        let later = SystemTime::now() + Duration::from_secs(3600);
        for ignored in [
            "local.settings.json",
            "src/readme.md",
            "src/node_modules/pkg/index.ts",
            "src/dist/out.ts",
            "dist/src/function_app.js",
        ] {
            set_modified(&root.join(ignored), later);
            assert!(!is_stale(root, &metadata), "{ignored} is not an input");
        }
        for input in [
            "package.json",
            "tsconfig.build.json",
            "src/function_app.ts",
            "src/nested/extra.mts",
        ] {
            let path = root.join(input);
            let before = modified(&path);
            set_modified(&path, later);
            assert!(is_stale(root, &metadata), "{input} is an input");
            set_modified(&path, before);
            assert!(!is_stale(root, &metadata));
        }

        tick();
        write(&root.join("src/function_app.ts"), "y");
        assert!(is_stale(root, &metadata), "an edited source");
        tick();
        write(&metadata, "{}");
        assert!(!is_stale(root, &metadata));
        tick();
        fs::remove_file(root.join("src/nested/extra.mts")).unwrap();
        assert!(is_stale(root, &metadata), "a removed source");
    }

    #[test]
    fn writes_only_changed_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = metadata_path(dir.path());
        assert!(write_if_changed(&path, "{\"a\": 1}").unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\": 1}");

        let old = SystemTime::now() - Duration::from_secs(3600);
        set_modified(&path, old);
        assert!(!write_if_changed(&path, "{\"a\": 1}").unwrap());
        assert!(
            modified(&path) < old + Duration::from_secs(60),
            "an unchanged file is left alone"
        );

        assert!(write_if_changed(&path, "{\"a\": 2}").unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\": 2}");
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temp files left"
        );
    }

    #[test]
    fn load_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = metadata_path(dir.path());
        assert!(matches!(load(&path), Err(MetadataError::Read { .. })));

        write(&path, r#"{"schemaVersion": "1.0", "functions": []}"#);
        let error = load(&path).unwrap_err();
        assert!(matches!(error, MetadataError::Invalid { .. }));
        assert!(
            error.to_string().contains("schema version `1.0`"),
            "{error}"
        );

        write(&path, r#"{"schemaVersion": "2.0", "functions": ["#);
        assert!(matches!(load(&path), Err(MetadataError::Invalid { .. })));

        write(
            &path,
            r#"{"schemaVersion": "2.0", "functions": [{"functionName": "hello",
                "delegateParameters": [{"name": "name", "type": "string", "optional": false,
                "hasDefault": false, "position": 0, "isFabricParameter": false}],
                "contextAudiences": []}]}"#,
        );
        let metadata = load(&path).unwrap();
        assert_eq!(metadata.function_names().collect::<Vec<_>>(), ["hello"]);
    }

    #[cfg(windows)]
    #[test]
    fn node_paths_have_no_verbatim_prefix() {
        assert_eq!(
            node_path(Path::new(r"\\?\C:\apps\functions")),
            Path::new(r"C:\apps\functions")
        );
        assert_eq!(
            node_path(Path::new(r"C:\apps\.\functions")),
            Path::new(r"C:\apps\functions")
        );
    }
}
