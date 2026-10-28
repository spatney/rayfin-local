//! The app model: `@microsoft/rayfin-core`'s analysis of `rayfin/data` (and `rayfin/storage`), run
//! with the project's own packages and cached in `.rayfin-local/cache/analysis.json` while its
//! inputs are unchanged.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Context as _;
use ring::digest::{Context, SHA256};
use rl_model::{AnalyzeInput, AppModel, Dialect};
use rl_node::{Helper, NodeRuntime};
use rl_project::config::Dialect as ConfigDialect;
use rl_project::{CORE_PACKAGE, LocalState, Project, write_atomic};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

use crate::ui;

const HELPER: Helper = Helper::new("analyze", rl_model::ANALYZE_JS);
const CACHE_FILE: &str = "analysis.json";
/// Bump when the cache's meaning changes.
const CACHE_VERSION: u32 = 1;

/// `analysis.json`.
#[derive(Serialize, Deserialize)]
struct CacheEntry {
    version: u32,
    /// [`analysis_key`] of the run that produced `analysis`.
    key: String,
    /// [`inputs_fingerprint`] of the files the TypeScript program read.
    inputs: String,
    analysis: Value,
}

/// Analyzes the project (or reuses the cached analysis), prints the analysis' diagnostics and
/// returns the model.
pub async fn analyze(
    project: &Project,
    state: &LocalState,
    node: &NodeRuntime,
    verbose: bool,
) -> anyhow::Result<AppModel> {
    let services = &project.config().services;
    if !services.data.enabled && !services.storage.enabled {
        tracing::debug!("services.data and services.storage are disabled; nothing to analyze");
        return Ok(empty_model(model_dialect(services.data.dialect())));
    }
    let hint = "Fix the error below, then run `rayfin-local serve` again.";
    let (model, cached) = load_model(project, state, node, verbose, hint).await?;
    let mut line = format!("Analyzed {}", summary(&model));
    if cached {
        line.push_str(" (cached)");
    }
    ui::step(line);
    Ok(model)
}

/// [`analyze`] again while serving, after a source changed: prints only the diagnostics.
pub async fn reanalyze(
    project: &Project,
    state: &LocalState,
    node: &NodeRuntime,
    verbose: bool,
) -> anyhow::Result<AppModel> {
    let hint = "Fix the error below and save again; the previous schema stays in use until then.";
    Ok(load_model(project, state, node, verbose, hint).await?.0)
}

/// What the model holds, such as `3 entities and 2 storage folders`.
pub fn summary(model: &AppModel) -> String {
    let entities = model.entities.len();
    let mut summary = format!(
        "{entities} {}",
        if entities == 1 { "entity" } else { "entities" }
    );
    if let Some(storage) = &model.storage {
        let folders = storage.folders.len();
        summary.push_str(&format!(
            " and {folders} storage {}",
            if folders == 1 { "folder" } else { "folders" }
        ));
    }
    summary
}

/// The model from the cache or a new analysis (`true` when cached), its diagnostics printed.
/// `hint` says what to do when the analysis fails.
async fn load_model(
    project: &Project,
    state: &LocalState,
    node: &NodeRuntime,
    verbose: bool,
    hint: &str,
) -> anyhow::Result<(AppModel, bool)> {
    let input = analyze_input(project, state, verbose);
    let key = analysis_key(project, &input);
    let cache_path = state.cache_dir().join(CACHE_FILE);
    let (mut model, cached) = match read_cache(&cache_path, &key, project.root()) {
        Some(model) => (model, true),
        None => (
            run_analysis(project, state, node, &input, key, &cache_path, hint).await?,
            false,
        ),
    };
    if !project.config().services.data.enabled {
        model.entities.clear();
    }
    for diagnostic in &model.diagnostics {
        ui::warn(&diagnostic.message);
    }
    Ok((model, cached))
}

async fn run_analysis(
    project: &Project,
    state: &LocalState,
    node: &NodeRuntime,
    input: &AnalyzeInput,
    key: String,
    cache_path: &Path,
    hint: &str,
) -> anyhow::Result<AppModel> {
    let analysis: Value = rl_node::run_helper(
        node,
        &HELPER,
        &state.cache_dir(),
        project.root(),
        &input.to_json(),
        ui::child_sink(),
    )
    .await
    .with_context(|| {
        format!(
            "Could not analyze the app's entities in {}. {hint}",
            input.data_dir.display()
        )
    })?;
    let model = AppModel::from_analysis(&analysis)?;
    let entry = CacheEntry {
        version: CACHE_VERSION,
        key,
        inputs: inputs_fingerprint(project.root(), &model.sources.inputs),
        analysis,
    };
    let written = serde_json::to_vec(&entry)
        .map_err(std::io::Error::other)
        .and_then(|bytes| write_atomic(cache_path, &bytes));
    if let Err(err) = written {
        tracing::debug!("could not write {}: {err}", cache_path.display());
    }
    Ok(model)
}

/// The helper's input: the project's entity and storage folders, compiled into
/// `.rayfin-local/cache/compiled`.
pub fn analyze_input(project: &Project, state: &LocalState, verbose: bool) -> AnalyzeInput {
    let services = &project.config().services;
    let mut input = AnalyzeInput::for_project(
        project.root(),
        model_dialect(services.data.dialect()),
        services.storage.enabled,
    );
    input.data_dir = project.data_dir();
    input.storage_dir = Some(project.storage_schema_dir());
    if let Some(rayfin_dir) = input.data_dir.parent() {
        let tsconfig = rayfin_dir.join("tsconfig.json");
        input.tsconfig_path = tsconfig.is_file().then_some(tsconfig);
    }
    input.out_dir = state.cache_dir().join("compiled");
    input.verbose = verbose;
    input
}

fn model_dialect(dialect: ConfigDialect) -> Dialect {
    match dialect {
        ConfigDialect::MsSql => Dialect::Mssql,
        ConfigDialect::PostgreSql => Dialect::Postgresql,
    }
}

fn empty_model(dialect: Dialect) -> AppModel {
    AppModel {
        core_version: None,
        typescript_version: None,
        dialect,
        feature_flags: None,
        entities: Vec::new(),
        storage: None,
        sources: Default::default(),
        diagnostics: Vec::new(),
    }
}

/// Everything the analysis depends on besides the files the TypeScript program reads.
fn analysis_key(project: &Project, input: &AnalyzeInput) -> String {
    let mut input_json = input.to_json();
    if let Some(fields) = input_json.as_object_mut() {
        fields.remove("verbose");
    }
    let mut files = vec![
        project.config_path(),
        project.data_service_root().join("package.json"),
    ];
    files.extend(input.tsconfig_path.clone());
    let core_version = project
        .rayfin_package(CORE_PACKAGE)
        .and_then(|package| package.version())
        .unwrap_or_default();
    let feature_flags = std::env::var("RAYFIN_FEATURE_FLAGS").unwrap_or_default();
    cache_key(
        &input_json,
        &[project.data_dir(), project.storage_schema_dir()],
        &files,
        &[rl_model::ANALYZE_JS, core_version, &feature_flags],
    )
}

/// SHA-256 over `input`, the path, size and modification time of every file under `dirs`, the
/// contents of `files` and `extra`.
fn cache_key(input: &Value, dirs: &[PathBuf], files: &[PathBuf], extra: &[&str]) -> String {
    let mut hasher = Hasher::new();
    hasher.field(&CACHE_VERSION.to_le_bytes());
    hasher.field(input.to_string().as_bytes());
    for dir in dirs {
        hasher.field(dir.to_string_lossy().as_bytes());
        if !dir.is_dir() {
            hasher.field(b"<missing>");
            continue;
        }
        let entries = WalkDir::new(dir).sort_by_file_name().into_iter();
        for entry in entries.filter_map(Result::ok) {
            if entry.file_type().is_file() {
                let relative = entry.path().strip_prefix(dir).unwrap_or(entry.path());
                hasher.field(relative.to_string_lossy().as_bytes());
                hasher.file_stamp(entry.path());
            }
        }
    }
    for file in files {
        hasher.field(file.to_string_lossy().as_bytes());
        match fs::read(file) {
            Ok(contents) => hasher.field(&contents),
            Err(_) => hasher.field(b"<missing>"),
        }
    }
    for value in extra {
        hasher.field(value.as_bytes());
    }
    hasher.finish()
}

/// The size and modification time of the project files the analysis read (`sources.inputs`,
/// relative to `root`), which may live outside `rayfin/data`.
fn inputs_fingerprint(root: &Path, inputs: &[String]) -> String {
    let mut hasher = Hasher::new();
    for input in inputs {
        hasher.field(input.as_bytes());
        hasher.file_stamp(&root.join(input));
    }
    hasher.finish()
}

fn read_cache(path: &Path, key: &str, root: &Path) -> Option<AppModel> {
    let bytes = fs::read(path).ok()?;
    let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
    if entry.version != CACHE_VERSION || entry.key != key {
        tracing::debug!("the analysis cache is stale");
        return None;
    }
    let model = AppModel::from_analysis(&entry.analysis).ok()?;
    if inputs_fingerprint(root, &model.sources.inputs) != entry.inputs {
        tracing::debug!("a file the analysis read has changed");
        return None;
    }
    tracing::debug!("reusing the analysis in {}", path.display());
    Some(model)
}

struct Hasher(Context);

impl Hasher {
    fn new() -> Self {
        Self(Context::new(&SHA256))
    }

    /// Adds one length-prefixed field, so that field boundaries are part of the hash.
    fn field(&mut self, bytes: &[u8]) {
        self.0.update(&(bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
    }

    fn file_stamp(&mut self, path: &Path) {
        match fs::metadata(path) {
            Ok(meta) => {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |since| since.as_nanos());
                self.field(&meta.len().to_le_bytes());
                self.field(&modified.to_le_bytes());
            }
            Err(_) => self.field(b"<missing>"),
        }
    }

    fn finish(self) -> String {
        hex::encode(self.0.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_of(root: &Path) -> String {
        let input = serde_json::json!({ "dataDir": "rayfin/data" });
        let dirs = [root.join("data"), root.join("storage")];
        cache_key(&input, &dirs, &[root.join("rayfin.yml")], &["helper"])
    }

    #[test]
    fn the_cache_key_changes_when_an_input_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::write(root.join("data").join("post.ts"), "export class Post {}").unwrap();
        fs::write(root.join("rayfin.yml"), "name: a").unwrap();
        let original = key_of(root);
        assert_eq!(key_of(root), original);

        fs::write(
            root.join("data").join("post.ts"),
            "export class Post { id }",
        )
        .unwrap();
        let edited = key_of(root);
        assert_ne!(edited, original);

        fs::write(root.join("data").join("user.ts"), "").unwrap();
        let added = key_of(root);
        assert_ne!(added, edited);

        fs::write(root.join("rayfin.yml"), "name: b").unwrap();
        assert_ne!(key_of(root), added);
    }

    #[test]
    fn the_inputs_fingerprint_notices_changed_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("shared.ts"), "export const a = 1;").unwrap();
        let inputs = ["shared.ts".to_owned()];
        let original = inputs_fingerprint(root, &inputs);
        fs::write(root.join("shared.ts"), "export const a = 12;").unwrap();
        let edited = inputs_fingerprint(root, &inputs);
        assert_ne!(edited, original);
        fs::remove_file(root.join("shared.ts")).unwrap();
        assert_ne!(inputs_fingerprint(root, &inputs), edited);
    }
}
