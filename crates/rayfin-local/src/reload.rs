//! Live reload while serving: when the entity or storage sources, other files they import, their
//! tsconfig or `rayfin.yml` change, the app is analyzed again, the database brought up to date
//! and the data API switched to the new model. Changes to the sources of a data package with a
//! build command (or of the workspace packages whose build output it imports) run the build
//! command first. A failed build or analysis, or a change that would lose data (without
//! `--force-schema`), leaves the previous model in use. Each analysis and its schema change are
//! reported to hosts (`analysis` and `schema` events).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use rl_db::Db;
use rl_graphql::DataApi;
use rl_model::Storage;
use rl_node::NodeRuntime;
use rl_project::{LocalState, Project, RayfinConfig};
use tokio::sync::mpsc;

use crate::analysis;
use crate::events::{self, Analysis, EntityChanges, Event, Schema};
use crate::integrations::ServeContext;
use crate::shutdown::StopHooks;
use crate::ui;

/// How long the sources must stay unchanged before a reload (editors save in bursts).
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Watches the project's model sources and `rayfin.yml`, until `hooks` run.
pub fn watch(ctx: &ServeContext<'_>, hooks: &mut StopHooks) -> anyhow::Result<()> {
    let sources = Sources::of(ctx.project);
    let (sender, receiver) = mpsc::unbounded_channel();
    let filter = sources.clone();
    let events = sender.clone();
    let watcher =
        notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
            Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                for path in &event.paths {
                    if let Some(change) = filter.classify(path, &event.kind) {
                        let _ = events.send(change);
                    }
                }
            }
            Ok(_) => {}
            Err(err) => tracing::debug!("watching the project: {err}"),
        })
        .context("Could not watch the project's files")?;

    let mut reloader = Reloader {
        project: ctx.project.clone(),
        state: ctx.state.clone(),
        node: ctx.node.clone(),
        db: ctx.db.clone(),
        data_api: ctx.data_api.clone(),
        storage: ctx.model.storage.clone(),
        force: ctx.options.force_schema,
        verbose: ctx.options.verbose,
        watcher,
        sources,
        watched: HashSet::new(),
    };
    reloader.watch_dir(&ctx.project.rayfin_dir(), RecursiveMode::NonRecursive)?;
    reloader.follow_folders(false)?;
    reloader.follow_files(&ctx.model.sources.inputs);
    // Changes saved since the analysis, before the watches above, left no event.
    if !ctx
        .model_stamp
        .is_current(ctx.project, ctx.state, ctx.model)
    {
        tracing::debug!("the model's sources changed since they were analyzed");
        let _ = sender.send(Change::Config);
        let _ = sender.send(Change::Model);
    }
    let task = tokio::spawn(reloader.run(receiver));
    hooks.add("live reload", move || async move {
        task.abort();
        let _ = task.await;
    });
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    /// `rayfin.yml`.
    Config,
    /// An entity or storage source, or another file the analysis read.
    Model,
    /// The entity or storage folder itself appeared, went or was renamed.
    Folder,
    /// A source of a package the data build command compiles.
    Package,
}

/// The files live reload follows.
#[derive(Clone)]
struct Sources {
    config: PathBuf,
    data_dir: PathBuf,
    storage_dir: PathBuf,
    /// The `rayfin/tsconfig.json` the analysis compiles with (it may not exist yet).
    tsconfig: PathBuf,
    /// The files outside the two folders that the last analysis read ([`Sources::outside_files`]),
    /// shared with the watcher's callback.
    files: Arc<RwLock<HashSet<PathBuf>>>,
    /// The packages the data build command compiles ([`packages`]), shared with the callback.
    packages: Arc<RwLock<Vec<Package>>>,
}

impl Sources {
    fn of(project: &Project) -> Self {
        Self {
            config: project.config_path(),
            data_dir: project.data_dir(),
            storage_dir: project.storage_schema_dir(),
            tsconfig: analysis::rayfin_tsconfig(project),
            files: Arc::default(),
            packages: Arc::default(),
        }
    }

    /// Whether `path` is in the entity or storage folder (or is one of them).
    fn in_folders(&self, path: &Path) -> bool {
        path.starts_with(&self.data_dir) || path.starts_with(&self.storage_dir)
    }

    fn classify(&self, path: &Path, kind: &EventKind) -> Option<Change> {
        let folder_moved = matches!(
            kind,
            EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
        );
        if path == self.config {
            Some(Change::Config)
        } else if folder_moved && (path == self.data_dir || path == self.storage_dir) {
            Some(Change::Folder)
        } else if self.in_folders(path)
            || self
                .files
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(path)
        {
            Some(Change::Model)
        } else if self
            .packages
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|package| package.has_source(path))
        {
            Some(Change::Package)
        } else {
            None
        }
    }

    /// The files outside the two folders that an analysis of `inputs` (`sources.inputs`, relative
    /// to `root`) depends on: imported modules, such as shared enums or a data package's build
    /// output, and the tsconfig with the configs it extends. Packages are left out.
    fn outside_files(&self, root: &Path, inputs: &[String]) -> HashSet<PathBuf> {
        let modules = inputs.iter().map(|input| {
            input
                .split('/')
                .fold(root.to_owned(), |path, part| path.join(part))
        });
        let tsconfigs = analysis::tsconfig_chain(&self.tsconfig);
        modules
            .chain(tsconfigs)
            .filter(|path| {
                !self.in_folders(path)
                    && !path
                        .components()
                        .any(|component| component.as_os_str() == "node_modules")
            })
            .collect()
    }
}

/// A package the data build command compiles. Its sources are its files outside `node_modules`,
/// dot-folders and `skipped`: its build output folders (and the functions, when they are in it).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Package {
    root: PathBuf,
    skipped: BTreeSet<PathBuf>,
}

impl Package {
    fn has_source(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        relative.components().next().is_some()
            && !self.skipped.iter().any(|dir| path.starts_with(dir))
            && !relative.components().any(|part| {
                let name = part.as_os_str().to_string_lossy();
                name == "node_modules" || name.starts_with('.')
            })
            && path.extension().is_none_or(|ext| ext != "tsbuildinfo")
    }

    /// The folders to watch for its sources: the package (not recursively), and the folders in it
    /// that may hold sources (recursively).
    fn folders(&self) -> Vec<(PathBuf, RecursiveMode)> {
        let mut folders = vec![(self.root.clone(), RecursiveMode::NonRecursive)];
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let name = entry.file_name();
            if dir.is_dir() && self.has_source(&dir) && name != "node_modules" {
                folders.push((dir, RecursiveMode::Recursive));
            }
        }
        folders
    }
}

/// The packages the data build command compiles, when `rayfin.yml` has one: the data package
/// (unless it is the project itself), and the workspace packages that hold files the analysis
/// read (`files`, such as their build output) other than the project's root package.
fn packages(project: &Project, files: &HashSet<PathBuf>) -> Vec<Package> {
    let data = &project.config().services.data;
    let has_build = data
        .build_command
        .as_deref()
        .is_some_and(|command| !command.trim().is_empty());
    if !data.enabled || !has_build {
        return Vec::new();
    }
    let root = project.root();
    let mut found: BTreeMap<PathBuf, BTreeSet<PathBuf>> = BTreeMap::new();
    let data_root = project.data_service_root();
    if data_root != root {
        let outputs = found.entry(data_root.to_owned()).or_default();
        if let Some(entry) = analysis::data_package_entry(project) {
            outputs.extend(top_folder(data_root, &entry));
        }
    }
    for file in files {
        let package = file
            .ancestors()
            .skip(1)
            .take_while(|dir| *dir != root && dir.starts_with(root))
            .find(|dir| dir.join("package.json").is_file());
        if let Some(package) = package {
            let outputs = found.entry(package.to_owned()).or_default();
            outputs.extend(top_folder(package, file));
        }
    }
    let functions = project.functions_dir();
    found
        .into_iter()
        .map(|(root, mut skipped)| {
            if functions.starts_with(&root) {
                skipped.insert(functions.to_owned());
            }
            Package { root, skipped }
        })
        .collect()
}

/// The folder right in `package` that holds `file`, unless `file` is right in `package`.
fn top_folder(package: &Path, file: &Path) -> Option<PathBuf> {
    let mut parts = file.strip_prefix(package).ok()?.components();
    let first = parts.next()?;
    parts.next()?;
    Some(package.join(first))
}

struct Reloader {
    /// The project as served (with `rayfin.yml` changes that apply live).
    project: Project,
    state: LocalState,
    node: NodeRuntime,
    db: Db,
    data_api: Option<Arc<DataApi>>,
    /// The storage folders being served, which only a restart changes.
    storage: Option<Storage>,
    /// `--force-schema`: apply changes that lose data.
    force: bool,
    verbose: bool,
    watcher: RecommendedWatcher,
    sources: Sources,
    /// The folders watched (recursively for the entity and storage folders).
    watched: HashSet<PathBuf>,
}

impl Reloader {
    async fn run(mut self, mut changes: mpsc::UnboundedReceiver<Change>) {
        let mut carried = Vec::new();
        loop {
            let mut batch = std::mem::take(&mut carried);
            if batch.is_empty() {
                let Some(first) = changes.recv().await else {
                    return;
                };
                batch.push(first);
            }
            while let Ok(Some(change)) = tokio::time::timeout(DEBOUNCE, changes.recv()).await {
                batch.push(change);
            }
            let build = batch.contains(&Change::Package);
            let mut analyze = build || batch.contains(&Change::Model);
            if batch.contains(&Change::Folder) {
                if let Err(err) = self.follow_folders(true) {
                    ui::warn(format_args!(
                        "{err:#}; restart rayfin-local after changing the entities."
                    ));
                }
                analyze = true;
            }
            if batch.contains(&Change::Config) {
                analyze |= self.reload_config();
            }
            if analyze {
                self.reload_model(build).await;
            }
            if build {
                // What the build wrote in the packages is no edit to build again for.
                while let Ok(Some(change)) = tokio::time::timeout(DEBOUNCE, changes.recv()).await {
                    if change != Change::Package {
                        carried.push(change);
                    }
                }
            }
        }
    }

    /// Watches `dir` unless it already is.
    fn watch_dir(&mut self, dir: &Path, mode: RecursiveMode) -> anyhow::Result<()> {
        if !self.watched.contains(dir) {
            self.watcher
                .watch(dir, mode)
                .with_context(|| format!("Could not watch {}", dir.display()))?;
            self.watched.insert(dir.to_owned());
        }
        Ok(())
    }

    /// Watches the entity and storage folders that exist (recursively), and the folders they are
    /// in, to see them appear. `renew` watches the two again: a folder that was removed or
    /// renamed and then created again needs a new watch.
    fn follow_folders(&mut self, renew: bool) -> anyhow::Result<()> {
        for dir in [
            self.sources.data_dir.clone(),
            self.sources.storage_dir.clone(),
        ] {
            if let Some(parent) = dir.parent()
                && parent.is_dir()
            {
                self.watch_dir(parent, RecursiveMode::NonRecursive)?;
            }
            if renew && self.watched.remove(&dir) {
                let _ = self.watcher.unwatch(&dir);
            }
            if dir.is_dir() {
                self.watch_dir(&dir, RecursiveMode::Recursive)?;
            }
        }
        Ok(())
    }

    /// Follows the files outside the two folders that an analysis of `inputs` read, watching the
    /// folders they are in (not recursively; other files there are ignored), and the sources of
    /// the packages the data build command compiles.
    fn follow_files(&mut self, inputs: &[String]) {
        let files = self.sources.outside_files(self.project.root(), inputs);
        let packages = packages(&self.project, &files);
        let dirs: BTreeSet<PathBuf> = files
            .iter()
            .filter_map(|file| file.parent())
            .filter(|dir| dir.is_dir())
            .map(Path::to_owned)
            .collect();
        for dir in dirs {
            if let Err(err) = self.watch_dir(&dir, RecursiveMode::NonRecursive) {
                ui::warn(format_args!(
                    "{err:#}; restart rayfin-local after changing the files the entities import \
                     from there."
                ));
            }
        }
        for (dir, mode) in packages.iter().flat_map(Package::folders) {
            if let Err(err) = self.watch_dir(&dir, mode) {
                ui::warn(format_args!(
                    "{err:#}; restart rayfin-local after changing the data package's sources."
                ));
            }
        }
        *self
            .sources
            .files
            .write()
            .unwrap_or_else(PoisonError::into_inner) = files;
        *self
            .sources
            .packages
            .write()
            .unwrap_or_else(PoisonError::into_inner) = packages;
    }

    /// Reads `rayfin.yml` again; returns whether the model must be analyzed again.
    fn reload_config(&mut self) -> bool {
        let project = match Project::load(self.project.root()) {
            Ok(project) => project,
            Err(err) => {
                ui::error(&anyhow::Error::new(err).context(
                    "rayfin.yml changed but could not be read; the previous configuration stays \
                     in use",
                ));
                return false;
            }
        };
        let change = ConfigChange::between(self.project.config(), project.config());
        if change.other {
            ui::warn("rayfin.yml changed — restart rayfin-local to apply");
            return false;
        }
        if change.dialect {
            self.project = project;
        }
        change.dialect
    }

    /// Analyzes the app again (`build`: after running the data build command) and switches to
    /// the new model.
    async fn reload_model(&mut self, build: bool) {
        let services = &self.project.config().services;
        if !services.data.enabled && !services.storage.enabled {
            return;
        }
        let reanalyzed =
            analysis::reanalyze(&self.project, &self.state, &self.node, self.verbose, build).await;
        events::emit(Event::Analysis(Analysis::of(
            &self.project,
            reanalyzed.as_ref(),
        )));
        let model = match reanalyzed {
            Ok(model) => model,
            Err(err) => {
                ui::error(&err);
                return;
            }
        };
        self.follow_files(&model.sources.inputs);
        if model.storage != self.storage {
            self.storage = model.storage.clone();
            ui::warn("The storage folders changed — restart rayfin-local to apply them.");
        }
        let Some(data_api) = &self.data_api else {
            return;
        };
        let served = data_api.model();
        if served.entities == model.entities && served.dialect == model.dialect {
            events::emit(Event::Schema(Schema::unchanged(true)));
            return;
        }
        match rl_db::apply(&self.db, &model, self.force).await {
            Ok(report) => {
                for warning in &report.warnings {
                    ui::warn(warning);
                }
                for loss in &report.forced {
                    ui::warn(format_args!("--force-schema: {loss}"));
                }
                let changes = EntityChanges::between(&served, &model);
                let mut summary = changes.summary();
                let steps = report.steps.len();
                if steps > 0 {
                    summary.push_str(&format!(
                        "; {steps} database {}",
                        if steps == 1 { "change" } else { "changes" }
                    ));
                }
                data_api.set_model(Arc::new(model));
                events::emit(Event::Schema(Schema::applied(
                    changes.items(steps),
                    &report,
                    true,
                )));
                ui::step(format_args!("Schema updated ({summary})"));
            }
            Err(err) => {
                events::emit(Event::Schema(Schema::rejected(&err, true)));
                ui::error(&anyhow::Error::new(err).context(
                    "The entity changes were not applied; the previous schema stays in use",
                ))
            }
        }
    }
}

/// How `rayfin.yml` changed: the dialect, which applies by analyzing again, and anything else,
/// which needs a restart.
#[derive(Debug, PartialEq, Eq)]
struct ConfigChange {
    dialect: bool,
    other: bool,
}

impl ConfigChange {
    fn between(served: &RayfinConfig, new: &RayfinConfig) -> Self {
        let without_dialect = |config: &RayfinConfig| {
            let mut config = config.clone();
            config.services.data.dialect = None;
            config.unknown_keys.clear();
            config
        };
        Self {
            dialect: served.services.data.dialect() != new.services.data.dialect(),
            other: without_dialect(served) != without_dialect(new),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> RayfinConfig {
        RayfinConfig::parse(
            yaml,
            &BTreeMap::new(),
            Path::new("rayfin.yml"),
            Path::new(".env"),
        )
        .unwrap()
    }

    #[test]
    fn only_dialect_changes_apply_without_a_restart() {
        let served = parse("services:\n  data:\n    enabled: true\n");
        let same =
            parse("# a comment\nservices:\n  data:\n    enabled: true\n    dialect: mssql\n");
        assert_eq!(
            ConfigChange::between(&served, &same),
            ConfigChange {
                dialect: false,
                other: false
            }
        );
        let dialect = parse("services:\n  data:\n    enabled: true\n    dialect: postgresql\n");
        assert_eq!(
            ConfigChange::between(&served, &dialect),
            ConfigChange {
                dialect: true,
                other: false
            }
        );
        let auth = parse("services:\n  data:\n    enabled: true\n  auth:\n    enabled: true\n");
        assert!(ConfigChange::between(&served, &auth).other);
    }

    #[test]
    fn changes_are_followed_in_the_model_folders_and_the_files_read_outside_them() {
        use notify::event::{CreateKind, RemoveKind};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let at = |path: &str| {
            path.split('/')
                .fold(root.to_owned(), |path, part| path.join(part))
        };
        std::fs::create_dir_all(at("rayfin")).unwrap();
        std::fs::write(
            at("rayfin/tsconfig.json"),
            r#"{"extends": "../tsconfig.json"}"#,
        )
        .unwrap();
        let sources = Sources {
            config: at("rayfin/rayfin.yml"),
            data_dir: at("rayfin/data"),
            storage_dir: at("rayfin/storage"),
            tsconfig: at("rayfin/tsconfig.json"),
            files: Arc::default(),
            packages: Arc::new(RwLock::new(vec![Package {
                root: at("packages/data"),
                skipped: BTreeSet::from([at("packages/data/dist")]),
            }])),
        };
        let inputs = [
            "rayfin/data/todo.ts",
            "shared/enums.ts",
            "node_modules/pkg/index.ts",
            "packages/data/dist/index.js",
        ]
        .map(str::to_owned);
        *sources.files.write().unwrap() = sources.outside_files(root, &inputs);

        let edit = EventKind::Modify(ModifyKind::Any);
        let cases = [
            ("rayfin/rayfin.yml", edit, Some(Change::Config)),
            ("rayfin/data/nested/todo.ts", edit, Some(Change::Model)),
            ("shared/enums.ts", edit, Some(Change::Model)),
            ("rayfin/tsconfig.json", edit, Some(Change::Model)),
            ("tsconfig.json", edit, Some(Change::Model)),
            ("packages/data/dist/index.js", edit, Some(Change::Model)),
            ("packages/data/src/todo.ts", edit, Some(Change::Package)),
            ("packages/data/package.json", edit, Some(Change::Package)),
            (
                "rayfin/data",
                EventKind::Create(CreateKind::Folder),
                Some(Change::Folder),
            ),
            (
                "rayfin/storage",
                EventKind::Remove(RemoveKind::Folder),
                Some(Change::Folder),
            ),
            ("shared/other.ts", edit, None),
            ("node_modules/pkg/index.ts", edit, None),
            ("rayfin/functions/index.ts", edit, None),
            ("packages/data/dist/todo.js", edit, None),
            ("packages/data/.tsbuildinfo", edit, None),
            ("packages/data/tsconfig.tsbuildinfo", edit, None),
            ("packages/data/node_modules/pkg/index.js", edit, None),
            ("packages/data", edit, None),
        ];
        for (path, kind, change) in cases {
            assert_eq!(sources.classify(&at(path), &kind), change, "{path}");
        }
    }

    #[test]
    fn the_data_build_follows_the_data_package_and_the_workspace_packages_it_imports() {
        let dir = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            "rayfin/rayfin.yml",
            "services:\n  data:\n    enabled: true\n    path: packages/data\n    \
             buildCommand: npm run build\n",
        );
        write("package.json", "{}");
        write(
            "packages/data/package.json",
            r#"{"exports": {".": {"import": "./dist/index.js"}}}"#,
        );
        write("packages/shared/package.json", "{}");
        let project = Project::load(dir.path()).unwrap();
        let at = |path: &str| {
            path.split('/')
                .fold(project.root().to_owned(), |path, part| path.join(part))
        };
        let files = HashSet::from([at("packages/shared/dist/index.js"), at("shared/enums.ts")]);
        assert_eq!(
            packages(&project, &files),
            [
                Package {
                    root: at("packages/data"),
                    skipped: BTreeSet::from([at("packages/data/dist")]),
                },
                Package {
                    root: at("packages/shared"),
                    skipped: BTreeSet::from([at("packages/shared/dist")]),
                },
            ]
        );
    }
}
