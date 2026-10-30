//! The app model: `@microsoft/rayfin-core`'s analysis of `rayfin/data` (and `rayfin/storage`), run
//! with the project's own packages and cached in `.rayfin-local/cache/analysis.json` while its
//! inputs are unchanged. A data package's `services.data.buildCommand` runs first, as in the CLI.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use ring::digest::{Context, SHA256};
use rl_model::{AnalyzeInput, AppModel, Dialect};
use rl_node::{Helper, NodeError, NodeRuntime, ProcessSpec};
use rl_project::config::Dialect as ConfigDialect;
use rl_project::{CORE_PACKAGE, LocalState, Project, SUPPORTED_SDK_VERSION, write_atomic};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walkdir::WalkDir;

use crate::ui;

const HELPER: Helper = Helper::new("analyze", rl_model::ANALYZE_JS);
const CACHE_FILE: &str = "analysis.json";
/// Bump when the cache's meaning changes.
const CACHE_VERSION: u32 = 2;
/// Leeway for file systems whose modification times lag the clock or are coarse (FAT: 2 s).
const MTIME_SLACK: Duration = Duration::from_secs(2);
/// A bound on `extends` chains, which may be cyclic.
const MAX_TSCONFIGS: usize = 16;

/// What an analysis was computed from: its [`analysis_key`] and the [`inputs_fingerprint`] of
/// the files it read (`None` when one of them changed while it ran).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourcesStamp(Option<(String, String)>);

impl SourcesStamp {
    fn now(project: &Project, state: &LocalState, model: &AppModel) -> Self {
        let key = analysis_key(project, &analyze_input(project, state, false));
        Self(Some((
            key,
            inputs_fingerprint(project.root(), &model.sources.inputs),
        )))
    }

    /// Whether the sources are still those `model` was analyzed from (`self` is its stamp).
    pub fn is_current(&self, project: &Project, state: &LocalState, model: &AppModel) -> bool {
        self.0.is_some() && *self == Self::now(project, state, model)
    }
}

/// `analysis.json`.
#[derive(Serialize, Deserialize)]
struct CacheEntry {
    version: u32,
    /// [`analysis_key`] of the run that produced `analysis`.
    key: String,
    /// [`inputs_fingerprint`] of the files the analysis read.
    inputs: String,
    analysis: Value,
}

/// What to do when the build or the analysis fails.
struct Advice {
    /// After an analysis error, which is printed below it.
    analysis: &'static str,
    /// How to try again once a failed build is fixed.
    retry: &'static str,
}

const AT_STARTUP: Advice = Advice {
    analysis: "Fix the error below, then run `rayfin-local serve` again.",
    retry: "then run `rayfin-local serve` again",
};

const WHILE_SERVING: Advice = Advice {
    analysis: "Fix the error below and save again; the previous schema stays in use until then.",
    retry: "then save again; the previous schema stays in use until then",
};

/// Builds the data package when it has a build command, analyzes the project (or reuses the
/// cached analysis), prints the analysis' diagnostics and returns the model and what it was
/// analyzed from.
pub async fn analyze(
    project: &Project,
    state: &LocalState,
    node: &NodeRuntime,
    verbose: bool,
) -> anyhow::Result<(AppModel, SourcesStamp)> {
    let services = &project.config().services;
    if !services.data.enabled && !services.storage.enabled {
        tracing::debug!("services.data and services.storage are disabled; nothing to analyze");
        let model = empty_model(model_dialect(services.data.dialect()));
        let stamp = SourcesStamp::now(project, state, &model);
        return Ok((model, stamp));
    }
    let (model, cached, stamp) =
        load_model(project, state, node, verbose, &AT_STARTUP, true).await?;
    print_diagnostics(&model);
    let mut line = format!("Analyzed {}", summary(&model));
    if cached {
        line.push_str(" (cached)");
    }
    ui::step(line);
    Ok((model, stamp))
}

/// [`analyze`] again while serving, after a source changed: prints only the diagnostics of a new
/// analysis (a cached one was printed before). `build` runs the data package's build command
/// first.
pub async fn reanalyze(
    project: &Project,
    state: &LocalState,
    node: &NodeRuntime,
    verbose: bool,
    build: bool,
) -> anyhow::Result<AppModel> {
    let (model, cached, _) =
        load_model(project, state, node, verbose, &WHILE_SERVING, build).await?;
    if !cached {
        print_diagnostics(&model);
    }
    Ok(model)
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

/// The model from the cache or a new analysis (`true` when cached) and what it was analyzed
/// from, its diagnostics printed. `build` runs the data package's build command first.
async fn load_model(
    project: &Project,
    state: &LocalState,
    node: &NodeRuntime,
    verbose: bool,
    advice: &Advice,
    build: bool,
) -> anyhow::Result<(AppModel, bool, SourcesStamp)> {
    let built = if build {
        build_data_package(project, advice).await?
    } else {
        None
    };
    let input = analyze_input(project, state, verbose);
    let key = analysis_key(project, &input);
    let cache_path = state.cache_dir().join(CACHE_FILE);
    let (mut model, cached, inputs) = match read_cache(&cache_path, &key, project.root()) {
        Some((model, inputs)) => (model, true, Some(inputs)),
        None => {
            let run = Run {
                input: &input,
                key: key.clone(),
                cache_path: &cache_path,
                advice,
                built,
            };
            let (model, inputs) = run_analysis(project, state, node, run).await?;
            (model, false, inputs)
        }
    };
    if !project.config().services.data.enabled {
        model.entities.clear();
    }
    let stamp = SourcesStamp(inputs.map(|inputs| (key, inputs)));
    Ok((model, cached, stamp))
}

fn print_diagnostics(model: &AppModel) {
    for diagnostic in &model.diagnostics {
        ui::warn(&diagnostic.message);
    }
}

/// One analysis to run: its input and key, where to cache it, what to do when it fails and when
/// the data package's build finished (`None` when none ran).
struct Run<'a> {
    input: &'a AnalyzeInput,
    key: String,
    cache_path: &'a Path,
    advice: &'a Advice,
    built: Option<SystemTime>,
}

/// Runs the analysis and caches it, unless a file it read changed while it ran. Returns the
/// model and the [`inputs_fingerprint`] (`None` when something changed).
async fn run_analysis(
    project: &Project,
    state: &LocalState,
    node: &NodeRuntime,
    run: Run<'_>,
) -> anyhow::Result<(AppModel, Option<String>)> {
    // The files read are known only afterwards; their stamps must predate this.
    let since = edits_count_from(SystemTime::now(), run.built);
    let failed = |err: anyhow::Error| {
        let err = err.context(format!(
            "Could not analyze {}. {}",
            analyzed_sources(project),
            run.advice.analysis
        ));
        match sdk_upgrade_note(project) {
            Some(note) => err.context(note),
            None => err,
        }
    };
    let analysis: Value = rl_node::run_helper(
        node,
        &HELPER,
        &state.cache_dir(),
        project.root(),
        &run.input.to_json(),
        ui::child_sink(),
    )
    .await
    .map_err(|err| failed(err.into()))?;
    let model = AppModel::from_analysis(&analysis).map_err(|err| failed(err.into()))?;
    let inputs = inputs_fingerprint(project.root(), &model.sources.inputs);
    if changed_since(project.root(), &model.sources.inputs, since) {
        tracing::debug!("a file changed while the analysis ran; not caching it");
        return Ok((model, None));
    }
    let entry = CacheEntry {
        version: CACHE_VERSION,
        key: run.key,
        inputs: inputs.clone(),
        analysis,
    };
    let written = serde_json::to_vec(&entry)
        .map_err(std::io::Error::other)
        .and_then(|bytes| write_atomic(run.cache_path, &bytes));
    if let Err(err) = written {
        tracing::debug!("could not write {}: {err}", run.cache_path.display());
    }
    Ok((model, Some(inputs)))
}

/// Runs `services.data.buildCommand` (when data is enabled) with the platform shell in the data
/// package, as the CLI does before it analyzes the entities, its output labelled `build`. Returns
/// when it finished, or `None` when there is no build command.
async fn build_data_package(
    project: &Project,
    advice: &Advice,
) -> anyhow::Result<Option<SystemTime>> {
    let data = &project.config().services.data;
    let Some(command) = data
        .build_command
        .as_deref()
        .map(str::trim)
        .filter(|command| data.enabled && !command.is_empty())
    else {
        return Ok(None);
    };
    let dir = project.data_service_root();
    let shown = display_path(project, dir);
    let retry = advice.retry;
    tracing::debug!(
        "building the data package: `{command}` in {}",
        dir.display()
    );
    let spec = ProcessSpec::shell("build", command, dir).sink(ui::child_sink());
    match rl_node::run_to_completion(spec).await {
        Ok(()) => {}
        Err(NodeError::Failed { status, .. }) => {
            let status = status
                .code()
                .map_or_else(|| status.to_string(), |code| format!("exit code {code}"));
            bail!(
                "The data build command `{command}` (services.data.buildCommand in \
                 rayfin/rayfin.yml) failed in {shown} with {status}. Fix the errors above (run \
                 `{command}` in {shown} to see them again), {retry}."
            );
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!("Could not run the data build command `{command}` in {shown}; {retry}.")
            });
        }
    }
    ui::step(format_args!(
        "Built the data package ({shown}: `{command}`)"
    ));
    Ok(Some(SystemTime::now()))
}

/// What the analysis reads, for messages: the data package (when the entities come from its
/// build output), else the entity folder, else the storage folder.
fn analyzed_sources(project: &Project) -> String {
    if !project.config().services.data.enabled {
        format!(
            "the storage folders in {}",
            display_path(project, &project.storage_schema_dir())
        )
    } else if data_package_entry(project).is_some() {
        format!(
            "the entities of the data package {}",
            display_path(project, project.data_service_root())
        )
    } else {
        format!(
            "the entities in {}",
            display_path(project, &project.data_dir())
        )
    }
}

/// `path` relative to the project root (with `/`), or as is outside it.
fn display_path(project: &Project, path: &Path) -> String {
    match path.strip_prefix(project.root()) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_owned(),
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.display().to_string(),
    }
}

/// What to do when the installed `@microsoft/rayfin-core` is not the 1.36 line rayfin-local
/// implements (`None` when it is, or when its version is unknown).
fn sdk_upgrade_note(project: &Project) -> Option<String> {
    let version = project.rayfin_package(CORE_PACKAGE)?.installed.as_deref()?;
    sdk_note(version)
}

fn sdk_note(version: &str) -> Option<String> {
    let installed = major_minor(version)?;
    if installed == SUPPORTED_SDK_VERSION {
        return None;
    }
    let (major, minor) = SUPPORTED_SDK_VERSION;
    let advice = if installed < SUPPORTED_SDK_VERSION {
        format!(" Upgrade the app's @microsoft/rayfin-* packages to {major}.{minor}.")
    } else {
        String::new()
    };
    Some(format!(
        "This app uses {CORE_PACKAGE} {version}; rayfin-local implements {major}.{minor}.x.{advice}"
    ))
}

/// The major and minor version of a version such as `1.34.0-beta.2`.
fn major_minor(version: &str) -> Option<(u64, u64)> {
    let mut parts = version.trim().split(['.', '-', '+']);
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
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
    let tsconfig = rayfin_tsconfig(project);
    input.tsconfig_path = tsconfig.is_file().then_some(tsconfig);
    input.out_dir = state.cache_dir().join("compiled");
    input.verbose = verbose;
    input
}

/// The `rayfin/tsconfig.json` (which may not exist) of the package whose `rayfin/` folder the
/// helper compiles: the storage package's when the entities come from the data package's build
/// output, else the data package's.
pub(crate) fn rayfin_tsconfig(project: &Project) -> PathBuf {
    let storage_only =
        project.config().services.storage.enabled && data_package_entry(project).is_some();
    let package = if storage_only {
        project.storage_service_root()
    } else {
        project.data_service_root()
    };
    package.join("rayfin").join("tsconfig.json")
}

/// The module the entities are imported from when the data package is loaded as built: the
/// `.js` entry of `exports`, else `main`, of its `package.json`, as the CLI's
/// `resolvePackageExports` picks it. `None` when the entities are compiled from `rayfin/data`.
pub(crate) fn data_package_entry(project: &Project) -> Option<PathBuf> {
    package_entry(project.data_service_root())
}

fn package_entry(dir: &Path) -> Option<PathBuf> {
    fn from_exports(exports: &Value) -> Option<&str> {
        let js = |entry: &&str| entry.ends_with(".js");
        match exports {
            Value::String(entry) => Some(entry.as_str()).filter(js),
            Value::Object(map) => match map.get(".") {
                Some(dot) => from_exports(dot),
                None => ["import", "require", "default"]
                    .iter()
                    .find_map(|key| map.get(*key).and_then(Value::as_str).filter(js)),
            },
            _ => None,
        }
    }
    let text = fs::read_to_string(dir.join("package.json")).ok()?;
    let package: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    let entry = package
        .get("exports")
        .and_then(from_exports)
        .or_else(|| package.get("main").and_then(Value::as_str))?;
    Some(normalize_path(&dir.join(entry)))
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
    files.extend(
        input
            .tsconfig_path
            .iter()
            .flat_map(|path| tsconfig_chain(path)),
    );
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
pub(crate) fn cache_key(
    input: &Value,
    dirs: &[PathBuf],
    files: &[PathBuf],
    extra: &[&str],
) -> String {
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

/// The contents of the project files the analysis read (`sources.inputs`, relative to `root`),
/// which may live outside `rayfin/data`: contents rather than modification times, so that a
/// build that writes the same output again leaves the cache valid.
pub(crate) fn inputs_fingerprint(root: &Path, inputs: &[String]) -> String {
    let mut hasher = Hasher::new();
    for input in inputs {
        hasher.field(input.as_bytes());
        match fs::read(root.join(input)) {
            Ok(contents) => hasher.field(&contents),
            Err(_) => hasher.field(b"<missing>"),
        }
    }
    hasher.finish()
}

/// From when modifications of the files an analysis starting at `started` reads may have been
/// missed by it: [`MTIME_SLACK`] earlier, but not before the build that just ran (`built`)
/// finished, as its output is complete.
fn edits_count_from(started: SystemTime, built: Option<SystemTime>) -> SystemTime {
    let slack = started.checked_sub(MTIME_SLACK).unwrap_or(UNIX_EPOCH);
    built.map_or(slack, |built| slack.max(built))
}

/// Whether a file in `inputs` was modified or removed since `since` ([`edits_count_from`]): the
/// analysis may have read the previous version. Modification times beyond now, which no edit
/// made, don't count.
fn changed_since(root: &Path, inputs: &[String], since: SystemTime) -> bool {
    let window = since..=SystemTime::now() + MTIME_SLACK;
    inputs.iter().any(|input| {
        fs::metadata(root.join(input))
            .and_then(|meta| meta.modified())
            .map_or(true, |modified| window.contains(&modified))
    })
}

/// `tsconfig` and the configs it extends, directly or not, as TypeScript resolves `extends`.
/// Configs that can't be read end the chain there.
pub(crate) fn tsconfig_chain(tsconfig: &Path) -> Vec<PathBuf> {
    let mut chain = vec![tsconfig.to_owned()];
    let mut next = 0;
    while let Some(config) = chain.get(next).cloned() {
        next += 1;
        let Some(json) = fs::read_to_string(&config)
            .ok()
            .and_then(|text| parse_jsonc(&text))
        else {
            continue;
        };
        let specifiers = match json.get("extends") {
            Some(Value::String(one)) => vec![one.as_str()],
            Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let dir = config.parent().unwrap_or(Path::new(""));
        for base in specifiers
            .into_iter()
            .filter_map(|specifier| resolve_extends(dir, specifier))
        {
            if chain.len() < MAX_TSCONFIGS && !chain.contains(&base) {
                chain.push(base);
            }
        }
    }
    chain
}

/// The config an `extends` entry of a config in `dir` names: a path (with `.json` added when the
/// file without it doesn't exist) or a config file in a package under `node_modules`.
fn resolve_extends(dir: &Path, specifier: &str) -> Option<PathBuf> {
    let specifier = specifier.replace('\\', "/");
    let with_json = |path: &Path| {
        let mut path = path.as_os_str().to_owned();
        path.push(".json");
        PathBuf::from(path)
    };
    if specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with('/')
        || Path::new(&specifier).is_absolute()
    {
        let config = normalize_path(&dir.join(&specifier));
        return Some(if config.is_file() || specifier.ends_with(".json") {
            config
        } else {
            with_json(&config)
        });
    }
    dir.ancestors().find_map(|ancestor| {
        let base = normalize_path(&ancestor.join("node_modules").join(&specifier));
        [base.clone(), with_json(&base), base.join("tsconfig.json")]
            .into_iter()
            .find(|candidate| candidate.is_file())
    })
}

/// `path` with `.` and `..` resolved lexically and native separators, as TypeScript does.
fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if normalized.file_name().is_some() => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// Parses JSON with comments and trailing commas, as TypeScript reads `tsconfig.json`.
fn parse_jsonc(text: &str) -> Option<Value> {
    let mut json = String::with_capacity(text.len());
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    // A comma is written once the next token shows it isn't a trailing one.
    let mut comma = false;
    while let Some(ch) = chars.next() {
        match ch {
            '/' if chars.peek() == Some(&'/') => {
                while chars.next_if(|&next| next != '\n').is_some() {}
                continue;
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    previous = next;
                }
                continue;
            }
            ',' if !comma => {
                comma = true;
                continue;
            }
            ch if ch.is_whitespace() => {
                json.push(ch);
                continue;
            }
            _ => {}
        }
        if std::mem::take(&mut comma) && !matches!(ch, '}' | ']') {
            json.push(',');
        }
        json.push(ch);
        if ch == '"' {
            while let Some(ch) = chars.next() {
                json.push(ch);
                match ch {
                    '\\' => json.extend(chars.next()),
                    '"' => break,
                    _ => {}
                }
            }
        }
    }
    serde_json::from_str(&json).ok()
}

fn read_cache(path: &Path, key: &str, root: &Path) -> Option<(AppModel, String)> {
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
    Some((model, entry.inputs))
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

    #[test]
    fn inputs_edited_while_the_analysis_ran_are_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("enums.ts");
        fs::write(&file, "export enum Status { Open }").unwrap();
        let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(an_hour_ago)
            .unwrap();
        let inputs = ["enums.ts".to_owned()];
        // File systems stamp writes with a coarser clock than `SystemTime::now`: the slack covers it.
        let since = edits_count_from(SystemTime::now(), None);
        assert!(!changed_since(root, &inputs, since));
        fs::write(&file, "export enum Status { Open, Done }").unwrap();
        assert!(changed_since(root, &inputs, since));
        assert!(changed_since(root, &["gone.ts".to_owned()], since));
    }

    #[test]
    fn the_inputs_fingerprint_follows_contents_not_rewrites() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("index.js");
        fs::write(&file, "export class Deck {}").unwrap();
        let inputs = ["index.js".to_owned()];
        let original = inputs_fingerprint(root, &inputs);
        let later = SystemTime::now() + Duration::from_secs(60);
        fs::write(&file, "export class Deck {}").unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(inputs_fingerprint(root, &inputs), original);
    }

    #[test]
    fn output_the_data_build_wrote_before_the_analysis_is_no_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("index.js");
        fs::write(&file, "export class Deck {}").unwrap();
        let started = SystemTime::now();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(started - Duration::from_millis(800))
            .unwrap();
        let inputs = ["index.js".to_owned()];
        assert!(changed_since(
            root,
            &inputs,
            edits_count_from(started, None)
        ));
        let built = Some(started - Duration::from_millis(500));
        assert!(!changed_since(
            root,
            &inputs,
            edits_count_from(started, built)
        ));
    }

    #[test]
    fn the_data_package_entry_follows_the_clis_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let entry_of = |package: &str| {
            fs::write(root.join("package.json"), package).unwrap();
            package_entry(root)
        };
        let dist = Some(root.join("dist").join("index.js"));
        assert_eq!(entry_of(r#"{"exports": "./dist/index.js"}"#), dist);
        assert_eq!(
            entry_of(r#"{"exports": {".": {"import": "./dist/index.js", "types": "x.d.ts"}}}"#),
            dist
        );
        assert_eq!(
            entry_of(r#"{"exports": {"default": "./dist/index.js"}}"#),
            dist
        );
        assert_eq!(
            entry_of(r#"{"exports": "./src/index.ts", "main": "dist/index.js"}"#),
            dist
        );
        assert_eq!(entry_of(r#"{"name": "app"}"#), None);
        fs::remove_file(root.join("package.json")).unwrap();
        assert_eq!(package_entry(root), None);
    }

    #[test]
    fn older_sdks_are_told_to_upgrade() {
        assert_eq!(sdk_note("1.36.2"), None);
        assert_eq!(sdk_note("1.36.0-alpha.1917"), None);
        assert_eq!(
            sdk_note("1.34.0").as_deref(),
            Some(
                "This app uses @microsoft/rayfin-core 1.34.0; rayfin-local implements 1.36.x. \
                 Upgrade the app's @microsoft/rayfin-* packages to 1.36."
            )
        );
        assert!(!sdk_note("1.37.1").unwrap().contains("Upgrade"));
    }

    /// A project with `rayfin.yml` and the files given (path, contents).
    fn project_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let project = Project::load(dir.path()).unwrap();
        (dir, project)
    }

    const PACKAGE_LAYOUT: &str = "services:\n  data:\n    enabled: true\n    path: packages/data\n    \
                                  buildCommand: exit 3\n  storage:\n    enabled: true\n";

    #[test]
    fn storage_compiles_with_its_packages_tsconfig_when_the_entities_are_built() {
        let (_dir, project) = project_with(&[
            ("rayfin/rayfin.yml", PACKAGE_LAYOUT),
            ("rayfin/tsconfig.json", "{}"),
            (
                "packages/data/package.json",
                r#"{"exports": "./dist/index.js"}"#,
            ),
        ]);
        let rayfin = project.root().join("rayfin");
        assert_eq!(rayfin_tsconfig(&project), rayfin.join("tsconfig.json"));
        assert_eq!(
            analyzed_sources(&project),
            "the entities of the data package packages/data"
        );

        fs::write(project.data_service_root().join("package.json"), "{}").unwrap();
        let data_rayfin = project.data_service_root().join("rayfin");
        assert_eq!(rayfin_tsconfig(&project), data_rayfin.join("tsconfig.json"));
        assert_eq!(
            analyzed_sources(&project),
            "the entities in packages/data/rayfin/data"
        );
    }

    #[tokio::test]
    async fn a_failed_data_build_names_the_command_and_its_exit_code() {
        let (_dir, project) = project_with(&[
            ("rayfin/rayfin.yml", PACKAGE_LAYOUT),
            ("packages/data/package.json", "{}"),
        ]);
        let err = build_data_package(&project, &AT_STARTUP)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with(
                "The data build command `exit 3` (services.data.buildCommand in \
                 rayfin/rayfin.yml) failed in packages/data with exit code 3."
            ),
            "{err}"
        );
    }

    #[test]
    fn the_tsconfig_chain_follows_extends() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write(
            "rayfin/tsconfig.json",
            "{\n  // shared settings\n  \"extends\": [\"../tsconfig.base\", \"@tsconfig/node20\"],\n  \
             \"compilerOptions\": { \"outDir\": \"//not-a-comment\", },\n}",
        );
        write(
            "tsconfig.base.json",
            r#"{"extends": "./rayfin/tsconfig.json"}"#,
        );
        write("node_modules/@tsconfig/node20/tsconfig.json", "{}");
        let tsconfig = root.join("rayfin").join("tsconfig.json");
        assert_eq!(
            tsconfig_chain(&tsconfig),
            [
                tsconfig.clone(),
                root.join("tsconfig.base.json"),
                root.join("node_modules")
                    .join("@tsconfig")
                    .join("node20")
                    .join("tsconfig.json"),
            ]
        );
    }
}
