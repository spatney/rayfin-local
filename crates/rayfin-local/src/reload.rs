//! Live reload while serving: changes to the project apply in place, on the same ports.
//!
//! - **Entities and storage folders.** When the entity or storage sources change (or other files
//!   they import, or their tsconfig), the app is analyzed again, the database brought up to date
//!   and the data API switched to the new model; the storage service is rebuilt when its folders
//!   change. Changes to the sources of a data package with a build command (or of the workspace
//!   packages whose build output it imports) run the build command first. A failed build or
//!   analysis, or a change that would lose data (without `--force-schema`), leaves the previous
//!   model in use.
//! - **`rayfin.yml` and `rayfin-local.toml`.** The feature services whose settings changed are
//!   rebuilt (the auth settings, the data API, functions, storage, connectors, the declared
//!   secrets, CORS) and the backend's router is replaced ([`RouterHandle`]): the listeners, the
//!   database, the sessions and the signing keys stay. Changes that can't apply while serving
//!   (where the frontend is, the ports, the token source) are reported as needing a restart
//!   ([`changes`]); a file that can't be read leaves the previous configuration in use.
//! - **The SQL connectors' entities** (`rayfin/connectors/`) are analyzed again, and the
//!   connectors' routes rebuilt.
//! - **The secret values** (`.rayfin-local/secrets.json`, the functions' `local.settings.json`):
//!   only the function host restarts, with the new values.
//!
//! Hosts see each step as events (`analysis`, `schema`, `functions`, then `config-changed` and a
//! new `ready`). [`ReloadHandle`] applies everything on demand.

mod changes;

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use notify::event::ModifyKind;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use rl_auth::AuthService;
use rl_db::{Db, MigrationReport};
use rl_graphql::DataApi;
use rl_model::AppModel;
use rl_node::NodeRuntime;
use rl_project::{LocalState, Project};
use rl_server::{RouterHandle, StaticSite};
use rl_tokens::TokenBroker;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use self::changes::ConfigChange;
use crate::analysis::{self, SourcesStamp};
use crate::commands::serve::{self, Features, ServeOptions};
use crate::errors::coded;
use crate::events::{
    self, Analysis, ConfigChanged, EntityChanges, ErrorCode, Event, Functions, FunctionsStatus,
    Schema, SchemaResult,
};
use crate::integrations::{self, SecretValues, ServeContext};
use crate::shutdown::StopHooks;
use crate::ui;

/// How long the sources must stay unchanged before a reload (editors save in bursts).
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Applies the project's changes on demand: `rayfin.yml`, `rayfin-local.toml`, the entities, the
/// SQL connectors' entities and the secret values are read again and applied as live reload
/// applies them after an edit. It is what the control API's `POST /reload` calls. Cheap to
/// clone; made by [`channel`] before the backend starts, and served once [`start`] runs.
#[derive(Clone)]
pub struct ReloadHandle {
    messages: mpsc::UnboundedSender<Message>,
}

impl fmt::Debug for ReloadHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReloadHandle").finish_non_exhaustive()
    }
}

/// What [`ReloadHandle::reload`] did: the events it emitted, as their lines have them (`v`,
/// `type` and `time`, then the fields). It serializes as `POST /reload` answers:
/// `{"schema": …, "config": …}`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReloadOutcome {
    /// The `schema` event: `unchanged`, `applied`, `refused` (it would lose data) or `failed`.
    pub schema: Value,
    /// The `config-changed` event, when the configuration or the secret values changed.
    pub config: Option<Value>,
}

impl ReloadHandle {
    /// Reads `rayfin.yml`, `rayfin-local.toml`, the entities, the SQL connectors' entities and
    /// the secret values again, now, and applies them. `force_schema` also applies a schema
    /// change that loses data, as `--force-schema` does. A reload in progress finishes first.
    /// Returns the `schema` event, and the `config-changed` event when something changed; both
    /// are emitted too.
    ///
    /// Fails, leaving the previous configuration and model in use, when `rayfin.yml` or
    /// `rayfin-local.toml` can't be read (code `config-invalid`) or the entities can't be
    /// analyzed (`analysis-failed`); `errors::HostError::classify` gives the code. The changes
    /// that could be read are still applied. Also fails while `serve` stops.
    #[allow(dead_code, reason = "called by the control API's POST /reload")]
    pub async fn reload(&self, force_schema: bool) -> anyhow::Result<ReloadOutcome> {
        let (reply, answer) = oneshot::channel();
        let request = Request {
            force: force_schema,
            reply,
        };
        self.messages
            .send(Message::Reload(request))
            .map_err(|_| stopping())?;
        answer.await.map_err(|_| stopping())?
    }
}

fn stopping() -> anyhow::Error {
    anyhow!("rayfin-local is stopping, so it can't reload the project.")
}

/// The requests of a [`ReloadHandle`], waiting for [`start`].
pub struct Requests(mpsc::UnboundedReceiver<Message>);

/// A [`ReloadHandle`], and its requests for [`start`] to serve (they wait until then).
pub fn channel() -> (ReloadHandle, Requests) {
    let (messages, requests) = mpsc::unbounded_channel();
    (ReloadHandle { messages }, Requests(requests))
}

/// What live reload gets from `serve` once the backend runs.
pub struct Serving {
    pub features: Features,
    /// The router the backend serves.
    pub router: RouterHandle,
    /// The app built for `--static`.
    pub static_site: Option<StaticSite>,
    /// Where the app is (`ready`'s `appUrl`).
    pub app_url: Option<String>,
}

/// Starts live reload: watches the project's files and serves `requests`, until `hooks` run.
/// When the files can't be watched, changes apply only on request.
pub fn start(ctx: &ServeContext<'_>, serving: Serving, requests: Requests, hooks: &mut StopHooks) {
    let Serving {
        features,
        router,
        static_site,
        app_url,
    } = serving;
    let mut reloader = Reloader {
        run: Run {
            state: ctx.state.clone(),
            node: ctx.node.clone(),
            db: ctx.db.clone(),
            auth: Arc::clone(ctx.auth),
            broker: Arc::clone(ctx.broker),
            options: ctx.options.clone(),
            host: ctx.host.to_owned(),
            api_origin: ctx.api_origin.to_owned(),
            frontend_port: ctx.frontend_port,
            model_stamp: ctx.model_stamp.clone(),
            reload: ctx.reload.clone(),
            router,
            static_site,
            app_url,
        },
        live: Live {
            project: ctx.project.clone(),
            model: Arc::clone(ctx.model),
            data_api: ctx.data_api.clone(),
            features,
        },
        watch: None,
        force: ctx.options.force_schema,
        verbose: ctx.options.verbose,
    };
    let messages = ctx.reload.messages.clone();
    match Watch::new(ctx.project, ctx.state, messages.clone()) {
        Ok(mut watch) => {
            watch.follow_files(ctx.project, &ctx.model.sources.inputs);
            reloader.watch = Some(watch);
        }
        Err(err) => ui::warn(format_args!(
            "Live reload is off ({err:#}); restart rayfin-local after changing the project."
        )),
    }
    // Changes saved since `serve` read them, before the watches above, left no event.
    if !ctx
        .model_stamp
        .is_current(ctx.project, ctx.state, ctx.model)
    {
        tracing::debug!("the model's sources changed since they were analyzed");
        let _ = messages.send(Message::Changed(Change::Model));
    }
    for change in [Change::Config, Change::Secrets] {
        let _ = messages.send(Message::Changed(change));
    }
    let task = tokio::spawn(reloader.run(requests.0));
    hooks.add("live reload", move || async move {
        task.abort();
        let _ = task.await;
    });
}

/// What the reloader is asked to do.
enum Message {
    /// A watched file changed.
    Changed(Change),
    /// [`ReloadHandle::reload`].
    #[allow(dead_code, reason = "sent by the control API's POST /reload")]
    Reload(Request),
}

struct Request {
    force: bool,
    reply: oneshot::Sender<anyhow::Result<ReloadOutcome>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    /// `rayfin.yml` or `rayfin-local.toml`.
    Config,
    /// An entity or storage source, or another file the analysis read.
    Model,
    /// The entity or storage folder itself appeared, went or was renamed.
    Folder,
    /// A source of a package the data build command compiles.
    Package,
    /// A file in the SQL connectors' folder; `folder`: the folder itself appeared, went or was
    /// renamed.
    Connectors { folder: bool },
    /// `.rayfin-local/secrets.json`, or the functions' `local.settings.json`.
    Secrets,
    /// The functions' folder appeared, or its `package.json` changed.
    Functions,
}

/// The changes of a burst of edits, or everything for a [`ReloadHandle::reload`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Batch {
    config: bool,
    model: bool,
    /// Run the data build command first.
    build: bool,
    folder: bool,
    connectors: bool,
    connectors_folder: bool,
    secrets: bool,
    functions: bool,
    /// Read everything again, and start what didn't start before.
    everything: bool,
}

impl Batch {
    fn add(&mut self, change: Change) {
        match change {
            Change::Config => self.config = true,
            Change::Model => self.model = true,
            Change::Folder => self.folder = true,
            Change::Package => self.build = true,
            Change::Connectors { folder } => {
                self.connectors = true;
                self.connectors_folder |= folder;
            }
            Change::Secrets => self.secrets = true,
            Change::Functions => self.functions = true,
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The files live reload follows.
#[derive(Clone)]
struct Sources {
    /// The paths the configuration decides, shared with the watcher's callback.
    paths: Arc<RwLock<Paths>>,
    /// The files outside the entity and storage folders that the last analysis read
    /// ([`Sources::outside_files`]), shared with the watcher's callback.
    files: Arc<RwLock<HashSet<PathBuf>>>,
    /// The packages the data build command compiles ([`packages`]), shared with the callback.
    packages: Arc<RwLock<Vec<Package>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Paths {
    config: PathBuf,
    local_config: PathBuf,
    data_dir: PathBuf,
    storage_dir: PathBuf,
    /// The `rayfin/tsconfig.json` the analysis compiles with (it may not exist yet).
    tsconfig: PathBuf,
    connectors_dir: PathBuf,
    secrets: PathBuf,
    functions_dir: PathBuf,
    /// The functions' `local.settings.json`.
    settings: PathBuf,
}

impl Paths {
    fn of(project: &Project, state: &LocalState) -> Self {
        let functions_dir = project.functions_dir().to_owned();
        Self {
            config: project.config_path(),
            local_config: project.local_config_path(),
            data_dir: project.data_dir(),
            storage_dir: project.storage_schema_dir(),
            tsconfig: analysis::rayfin_tsconfig(project),
            connectors_dir: project.rayfin_dir().join("connectors"),
            secrets: state.secrets_path(),
            settings: functions_dir.join("local.settings.json"),
            functions_dir,
        }
    }

    /// Whether `path` is in the entity or storage folder (or is one of them).
    fn in_folders(&self, path: &Path) -> bool {
        path.starts_with(&self.data_dir) || path.starts_with(&self.storage_dir)
    }
}

impl Sources {
    fn of(project: &Project, state: &LocalState) -> Self {
        Self {
            paths: Arc::new(RwLock::new(Paths::of(project, state))),
            files: Arc::default(),
            packages: Arc::default(),
        }
    }

    fn paths(&self) -> Paths {
        self.paths
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn classify(&self, path: &Path, kind: &EventKind) -> Option<Change> {
        let moved = matches!(
            kind,
            EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
        );
        let paths = self.paths.read().unwrap_or_else(PoisonError::into_inner);
        if path == paths.config || path == paths.local_config {
            Some(Change::Config)
        } else if path == paths.secrets || path == paths.settings {
            Some(Change::Secrets)
        } else if (moved && path == paths.functions_dir)
            || path == paths.functions_dir.join("package.json")
        {
            Some(Change::Functions)
        } else if path.starts_with(&paths.connectors_dir) {
            Some(Change::Connectors {
                folder: moved && path == paths.connectors_dir,
            })
        } else if moved && (path == paths.data_dir || path == paths.storage_dir) {
            Some(Change::Folder)
        } else if paths.in_folders(path)
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
        let paths = self.paths();
        let modules = inputs.iter().map(|input| {
            input
                .split('/')
                .fold(root.to_owned(), |path, part| path.join(part))
        });
        let tsconfigs = analysis::tsconfig_chain(&paths.tsconfig);
        modules
            .chain(tsconfigs)
            .filter(|path| {
                !paths.in_folders(path)
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

/// The watcher, and the folders it watches.
struct Watch {
    watcher: RecommendedWatcher,
    sources: Sources,
    /// The folders watched (recursively for the entity, storage and connector folders).
    watched: HashSet<PathBuf>,
}

impl Watch {
    /// Watches `project`'s configuration, its model and connector folders and the secret values;
    /// the changes go to `messages`.
    fn new(
        project: &Project,
        state: &LocalState,
        messages: mpsc::UnboundedSender<Message>,
    ) -> anyhow::Result<Self> {
        let sources = Sources::of(project, state);
        let filter = sources.clone();
        let watcher =
            notify::recommended_watcher(
                move |result: notify::Result<notify::Event>| match result {
                    Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                        for path in &event.paths {
                            if let Some(change) = filter.classify(path, &event.kind) {
                                let _ = messages.send(Message::Changed(change));
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(err) => tracing::debug!("watching the project: {err}"),
                },
            )
            .context("Could not watch the project's files")?;
        let mut watch = Self {
            watcher,
            sources,
            watched: HashSet::new(),
        };
        // `rayfin.yml` (and the connectors' folder appearing), `rayfin-local.toml`, and
        // `secrets.json`.
        for dir in [
            project.rayfin_dir(),
            project.root().to_owned(),
            state.dir().to_owned(),
        ] {
            watch.watch_dir(&dir, RecursiveMode::NonRecursive)?;
        }
        watch.follow_folders(false)?;
        watch.follow_connectors(false);
        watch.follow_settings();
        Ok(watch)
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

    /// Watches `dir` (recursively) when it exists. `renew` watches it again: a folder that was
    /// removed or renamed and then created again needs a new watch.
    fn follow_dir(&mut self, dir: &Path, renew: bool) -> anyhow::Result<()> {
        if renew && self.watched.remove(dir) {
            let _ = self.watcher.unwatch(dir);
        }
        if dir.is_dir() {
            self.watch_dir(dir, RecursiveMode::Recursive)?;
        }
        Ok(())
    }

    /// Watches the entity and storage folders that exist (recursively), and the folders they are
    /// in, to see them appear.
    fn follow_folders(&mut self, renew: bool) -> anyhow::Result<()> {
        let paths = self.sources.paths();
        for dir in [paths.data_dir, paths.storage_dir] {
            if let Some(parent) = dir.parent()
                && parent.is_dir()
            {
                self.watch_dir(parent, RecursiveMode::NonRecursive)?;
            }
            self.follow_dir(&dir, renew)?;
        }
        Ok(())
    }

    /// Watches the SQL connectors' folder (recursively) when it exists.
    fn follow_connectors(&mut self, renew: bool) {
        let dir = self.sources.paths().connectors_dir;
        if let Err(err) = self.follow_dir(&dir, renew) {
            ui::warn(format_args!(
                "{err:#}; restart rayfin-local after changing the connectors' entities."
            ));
        }
    }

    /// Watches the functions' folder (for `local.settings.json` and `package.json`), when it
    /// exists.
    fn follow_settings(&mut self) {
        let dir = self.sources.paths().functions_dir;
        if dir.is_dir()
            && let Err(err) = self.watch_dir(&dir, RecursiveMode::NonRecursive)
        {
            ui::warn(format_args!(
                "{err:#}; restart rayfin-local after changing local.settings.json."
            ));
        }
    }

    /// Follows the folders of a changed configuration (`services.*.path`).
    fn follow_project(&mut self, project: &Project, state: &LocalState) {
        let paths = Paths::of(project, state);
        if self.sources.paths() == paths {
            return;
        }
        *self
            .sources
            .paths
            .write()
            .unwrap_or_else(PoisonError::into_inner) = paths;
        if let Err(err) = self.follow_folders(false) {
            ui::warn(format_args!(
                "{err:#}; restart rayfin-local after changing the entities."
            ));
        }
        self.follow_connectors(false);
        self.follow_settings();
    }

    /// Follows the files outside the two folders that an analysis of `inputs` read, watching the
    /// folders they are in (not recursively; other files there are ignored), and the sources of
    /// the packages the data build command compiles.
    fn follow_files(&mut self, project: &Project, inputs: &[String]) {
        let files = self.sources.outside_files(project.root(), inputs);
        let packages = packages(project, &files);
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
}

/// What live reload rebuilds the services from: fixed for the run.
struct Run {
    state: LocalState,
    node: NodeRuntime,
    db: Db,
    auth: Arc<AuthService>,
    broker: Arc<TokenBroker>,
    options: ServeOptions,
    host: String,
    api_origin: String,
    frontend_port: Option<u16>,
    model_stamp: SourcesStamp,
    reload: ReloadHandle,
    router: RouterHandle,
    static_site: Option<StaticSite>,
    app_url: Option<String>,
}

/// What runs, as live reload changes it.
struct Live {
    /// The project as served (with the configuration changes that applied).
    project: Project,
    /// The model the data API and storage serve.
    model: Arc<AppModel>,
    /// `None` while `services.data` is off (or its schema was never applied).
    data_api: Option<Arc<DataApi>>,
    features: Features,
}

struct Reloader {
    run: Run,
    live: Live,
    /// `None` when the files can't be watched.
    watch: Option<Watch>,
    /// `--force-schema`: apply changes that lose data.
    force: bool,
    verbose: bool,
}

/// What a reload changes.
#[derive(Debug, Default)]
struct Update {
    /// What changed (`config-changed`'s `what`).
    what: Vec<&'static str>,
    /// What of it applied; a new `ready` follows when anything did.
    applied: Vec<&'static str>,
    /// Why some changes need a restart (`config-changed`'s `message`).
    restart: Vec<String>,
    /// The data or storage settings changed: analyze again.
    analyze: bool,
    /// The data API's settings changed.
    renew_data_api: bool,
    connectors: bool,
    functions: bool,
    secrets: bool,
    /// `[fabric]` changed: check the anonymous grants again.
    fabric: bool,
}

impl Update {
    fn note(&mut self, what: &'static str) {
        if !self.what.contains(&what) {
            self.what.push(what);
        }
    }

    fn apply(&mut self, what: &'static str) {
        self.note(what);
        if !self.applied.contains(&what) {
            self.applied.push(what);
        }
    }
}

/// The events a reload emitted.
struct Applied {
    schema: Option<Value>,
    config: Option<Value>,
}

/// What [`Reloader::reload_model`] changed.
#[derive(Default)]
struct ModelReload {
    /// The `schema` event it emitted.
    schema: Option<Value>,
    /// The data API was turned on or off, or replaced.
    data_api: bool,
    /// The storage service was rebuilt: its folders changed.
    storage: bool,
}

impl Reloader {
    async fn run(mut self, mut messages: mpsc::UnboundedReceiver<Message>) {
        let mut carried: VecDeque<Message> = VecDeque::new();
        loop {
            let mut batch = Batch::default();
            let mut request = None;
            let mut pending = std::mem::take(&mut carried);
            loop {
                let message = match pending.pop_front() {
                    Some(message) => message,
                    // A reload on request goes at once, with what has come in already.
                    None if request.is_some() => match messages.try_recv() {
                        Ok(message) => message,
                        Err(_) => break,
                    },
                    None if batch.is_empty() => match messages.recv().await {
                        Some(message) => message,
                        None => return,
                    },
                    // Editors save in bursts: wait until the changes settle.
                    None => match tokio::time::timeout(DEBOUNCE, messages.recv()).await {
                        Ok(Some(message)) => message,
                        Ok(None) | Err(_) => break,
                    },
                };
                match message {
                    Message::Changed(change) => batch.add(change),
                    Message::Reload(next) if request.is_none() => request = Some(next),
                    other => carried.push_back(other),
                }
            }
            let built = batch.build;
            match request {
                Some(Request { force, reply }) => {
                    let batch = Batch {
                        everything: true,
                        ..batch
                    };
                    let outcome = self.apply(batch, force || self.force).await;
                    let outcome = outcome.map(|applied| ReloadOutcome {
                        schema: applied.schema.unwrap_or_else(|| {
                            events::emit_line(Event::Schema(Schema::unchanged(true)))
                        }),
                        config: applied.config,
                    });
                    let _ = reply.send(outcome);
                }
                None => {
                    let _ = self.apply(batch, self.force).await;
                }
            }
            if built {
                // What the build wrote in the packages is no edit to build again for.
                while let Ok(Some(message)) = tokio::time::timeout(DEBOUNCE, messages.recv()).await
                {
                    if !matches!(message, Message::Changed(Change::Package)) {
                        carried.push_back(message);
                    }
                }
            }
        }
    }

    /// What the services are built from now.
    fn ctx(&self) -> ServeContext<'_> {
        ServeContext {
            project: &self.live.project,
            state: &self.run.state,
            node: &self.run.node,
            model: &self.live.model,
            model_stamp: &self.run.model_stamp,
            db: &self.run.db,
            auth: &self.run.auth,
            broker: &self.run.broker,
            data_api: self.live.data_api.clone(),
            options: &self.run.options,
            host: &self.run.host,
            api_origin: &self.run.api_origin,
            frontend_port: self.run.frontend_port,
            reload: self.run.reload.clone(),
        }
    }

    /// Applies `batch` (`force`: changes that lose data too), then reports what changed. Fails
    /// when the configuration can't be read or the model analyzed, after applying the rest.
    async fn apply(&mut self, batch: Batch, force: bool) -> anyhow::Result<Applied> {
        let everything = batch.everything;
        if let Some(watch) = &mut self.watch {
            if batch.folder
                && let Err(err) = watch.follow_folders(true)
            {
                ui::warn(format_args!(
                    "{err:#}; restart rayfin-local after changing the entities."
                ));
            }
            if batch.connectors_folder {
                watch.follow_connectors(true);
            }
            if batch.functions {
                watch.follow_settings();
            }
        }
        let mut update = Update::default();
        let mut failure = None;
        if (batch.config || everything)
            && let Err(err) = self.reload_config(&mut update)
        {
            failure = Some(err);
        }
        let mut schema = None;
        if batch.model || batch.build || batch.folder || everything || update.analyze {
            match self
                .reload_model(batch.build, force, update.renew_data_api)
                .await
            {
                Ok(reloaded) => {
                    schema = reloaded.schema;
                    if reloaded.data_api {
                        update.apply("data");
                    }
                    if reloaded.storage {
                        update.apply("storage");
                    }
                }
                Err(err) => {
                    failure.get_or_insert(err);
                }
            }
        }
        if batch.connectors || everything || update.connectors {
            self.reload_connectors(&mut update).await;
        }
        // Functions that failed to start (their folder was missing, say) are tried again.
        let retry_functions =
            (batch.config || batch.functions || everything) && self.functions_missing();
        if update.functions || retry_functions {
            self.restart_functions(&mut update).await;
        } else if batch.secrets || everything || update.secrets {
            self.reload_secrets(&mut update).await;
        }
        if update.fabric
            && let Err(err) = serve::check_anonymous_access(&self.live.project, &self.live.model)
        {
            ui::error_event(&coded(ErrorCode::AnalysisFailed, events::no_details(), err));
        }
        if !update.applied.is_empty() {
            self.rebuild_router();
        }
        let config = self.report(&update);
        match failure {
            Some(err) => Err(err),
            None => Ok(Applied { schema, config }),
        }
    }

    /// Reads `rayfin.yml` and `rayfin-local.toml` again. A change that applies while serving
    /// replaces the served project, and `update` says what to rebuild; otherwise it says why a
    /// restart is needed. A file that can't be read is reported (`config-invalid`), and the
    /// previous configuration stays in use.
    fn reload_config(&mut self, update: &mut Update) -> anyhow::Result<()> {
        let project = Project::load(self.live.project.root()).map_err(|err| {
            let err = coded(
                ErrorCode::ConfigInvalid,
                events::no_details(),
                anyhow::Error::new(err).context(
                    "The project's configuration changed but can't be read; the previous \
                     configuration stays in use",
                ),
            );
            ui::error_event(&err);
            err
        })?;
        let change = ConfigChange::between(
            &self.live.project,
            &project,
            &self.run.options,
            self.run.frontend_port.is_some(),
        );
        for &what in &change.what {
            update.note(what);
        }
        if !change.restart.is_empty() {
            update.restart.extend(change.restart);
            return Ok(());
        }
        if change.what.is_empty() {
            return Ok(());
        }
        for warning in project.warnings() {
            if !self.live.project.warnings().contains(warning) {
                ui::warn(warning);
            }
        }
        self.live.project = project;
        for &what in &change.what {
            update.apply(what);
        }
        if change.has("auth") || change.has("name") {
            self.apply_auth_settings();
        }
        update.analyze |= change.has("data") || change.has("storage");
        update.renew_data_api |= change.has("data");
        update.connectors |= change.has("connectors");
        update.functions |= change.has("functions");
        update.secrets |= change.has("secrets");
        update.fabric |= change.has("fabric");
        if let Some(watch) = &mut self.watch {
            watch.follow_project(&self.live.project, &self.run.state);
        }
        Ok(())
    }

    /// `services.auth` (and the app's name) to the auth service, which keeps its users,
    /// sessions and keys.
    fn apply_auth_settings(&self) {
        let settings = integrations::auth_settings(
            &self.live.project,
            &self.run.state.data().app_id,
            integrations::is_loopback(&self.run.host),
        );
        if let Err(err) = self.run.auth.set_settings(settings) {
            ui::error_event(&anyhow::Error::new(err).context(
                "Could not apply services.auth of rayfin.yml; the previous auth settings stay in \
                 use",
            ));
        }
    }

    /// Analyzes the app again (`build`: after running the data build command), brings the
    /// database to the new model (`force`: also when that loses data) and switches the data API
    /// and storage to it; `renew_api` replaces the data API (its settings changed). A refused or
    /// failed schema change keeps the previous model, storage folders included.
    async fn reload_model(
        &mut self,
        build: bool,
        force: bool,
        renew_api: bool,
    ) -> anyhow::Result<ModelReload> {
        let project = &self.live.project;
        let reanalyzed = analysis::reanalyze(
            project,
            &self.run.state,
            &self.run.node,
            self.verbose,
            build,
        )
        .await;
        events::emit(Event::Analysis(Analysis::of(project, reanalyzed.as_ref())));
        let model = match reanalyzed {
            Ok(model) => Arc::new(model),
            Err(err) => {
                ui::error(&err);
                return Err(coded(ErrorCode::AnalysisFailed, events::no_details(), err));
            }
        };
        if let Some(watch) = &mut self.watch {
            watch.follow_files(&self.live.project, &model.sources.inputs);
        }
        let mut reloaded = ModelReload::default();
        let schema = if self.live.project.config().services.data.enabled {
            let served = self.live.data_api.as_ref().map(|api| api.model());
            let same = served.as_ref().is_some_and(|served| {
                served.entities == model.entities && served.dialect == model.dialect
            });
            let schema = if same {
                Schema::unchanged(true)
            } else {
                match rl_db::apply(&self.run.db, &model, force).await {
                    Ok(report) => schema_applied(served.as_deref(), &model, &report),
                    Err(err) => {
                        reloaded.schema = Some(events::emit_line(Event::Schema(Schema::rejected(
                            &err, true,
                        ))));
                        ui::error(&anyhow::Error::new(err).context(
                            "The entity changes were not applied; the previous schema stays in \
                             use",
                        ));
                        return Ok(reloaded);
                    }
                }
            };
            match &self.live.data_api {
                Some(api) if !renew_api => {
                    if !same {
                        api.set_model(Arc::clone(&model));
                    }
                }
                _ => {
                    self.live.data_api = integrations::data_api(
                        &self.live.project,
                        &self.run.db,
                        &model,
                        self.run.auth.clone(),
                    );
                    reloaded.data_api = true;
                }
            }
            schema
        } else {
            // The tables stay, as when `serve` starts with the data service off.
            reloaded.data_api = self.live.data_api.take().is_some();
            Schema::unchanged(true)
        };
        reloaded.schema = Some(events::emit_line(Event::Schema(schema)));
        let storage_changed = model.storage != self.live.model.storage;
        self.live.model = model;
        if storage_changed {
            match integrations::storage_router(&self.ctx()).await {
                Ok(router) => {
                    self.live.features.storage = router;
                    reloaded.storage = true;
                }
                Err(err) => ui::error_event(&err),
            }
        }
        Ok(reloaded)
    }

    /// Analyzes the SQL connectors' entities again, and rebuilds the connectors' routes when
    /// they or the connectors' settings (`update.connectors`) changed.
    async fn reload_connectors(&mut self, update: &mut Update) {
        let models = integrations::connector_models(&self.ctx()).await;
        if !update.connectors && models == self.live.features.connector_models {
            return;
        }
        let router = integrations::connectors_router(&self.ctx(), models.clone());
        self.live.features.connectors = router;
        self.live.features.connector_models = models;
        if update.connectors {
            integrations::warn_unsupported_connectors(&self.live.project.config().connectors);
        }
        update.apply("connectors");
    }

    /// Whether the functions should run but don't (they could not start).
    fn functions_missing(&self) -> bool {
        !self.run.options.no_functions
            && self.live.project.config().services.functions.enabled
            && self.live.features.functions.get().is_none()
    }

    /// Stops the functions, and starts them again with the current settings unless they are off
    /// now.
    async fn restart_functions(&mut self, update: &mut Update) {
        update.apply("functions");
        if let Some(running) = self.live.features.functions.replace(None) {
            running.stop().await;
        }
        let (started, secrets) = {
            let ctx = self.ctx();
            let secrets = SecretValues::read(&ctx);
            (integrations::start_functions(&ctx, &secrets).await, secrets)
        };
        match started {
            Ok(Some(service)) => {
                self.live.features.functions.replace(Some(service));
            }
            Ok(None) => events::emit(Event::Functions(Functions {
                state: FunctionsStatus::Off,
                names: Vec::new(),
                errors: Vec::new(),
                message: Some("services.functions is disabled in rayfin.yml.".to_owned()),
            })),
            Err(err) => ui::error_event(&err),
        }
        self.live.features.secrets = secrets;
    }

    /// Reads the secret values again; when they changed, restarts only the function host, with
    /// them.
    async fn reload_secrets(&mut self, update: &mut Update) {
        let secrets = SecretValues::read(&self.ctx());
        if secrets == self.live.features.secrets {
            return;
        }
        update.apply("secrets");
        if let Some(service) = self.live.features.functions.get() {
            secrets.warn_missing(&self.ctx());
            ui::status("Restarting the function host with the new secret values…");
            service.restart_host(secrets.declared.clone()).await;
        }
        self.live.features.secrets = secrets;
    }

    /// Serves the current services from now on, on the same listeners.
    fn rebuild_router(&self) {
        let ctx = self.ctx();
        let config = serve::server_config(&ctx, self.run.static_site.clone());
        self.run.router.replace(rl_server::build_router(
            config,
            self.live.features.routers(&ctx),
        ));
    }

    /// Tells what changed: `config-changed`, then (when something applied) the rows of the
    /// banner that changed and a new `ready`. Returns the `config-changed` event.
    fn report(&self, update: &Update) -> Option<Value> {
        if update.what.is_empty() && update.restart.is_empty() {
            return None;
        }
        let message = (!update.restart.is_empty()).then(|| update.restart.join(" "));
        let event = events::emit_line(Event::ConfigChanged(ConfigChanged {
            applied: !update.applied.is_empty(),
            restart_required: message.is_some(),
            what: update.what.iter().map(|what| (*what).to_owned()).collect(),
            message: message.clone(),
        }));
        if let Some(message) = &message {
            ui::warn(message);
        }
        if !update.applied.is_empty() {
            let ctx = self.ctx();
            let settings = ctx.auth.settings();
            ui::step(format_args!(
                "Applied the changes ({})",
                update.applied.join(", ")
            ));
            let labels = row_labels(&update.applied);
            let rows: Vec<_> = serve::banner_rows(
                &ctx,
                &self.live.features,
                self.run.app_url.as_deref(),
                &settings,
            )
            .into_iter()
            .filter(|(label, _)| labels.contains(label))
            .collect();
            ui::rows(&rows);
            let ready = serve::ready_event(
                &ctx,
                &self.live.features,
                self.run.app_url.clone(),
                &settings,
            );
            events::emit(Event::Ready(Box::new(ready)));
        }
        Some(event)
    }
}

/// The `schema` event of a migration to `model` from `served` (`None` when no model was served),
/// with its warnings and status line.
fn schema_applied(served: Option<&AppModel>, model: &AppModel, report: &MigrationReport) -> Schema {
    for warning in &report.warnings {
        ui::warn(warning);
    }
    for loss in &report.forced {
        ui::warn(format_args!("--force-schema: {loss}"));
    }
    let changes = match served {
        Some(served) => EntityChanges::between(served, model),
        None => EntityChanges::of_steps(&report.steps, model),
    };
    let steps = report.steps.len();
    let schema = Schema::applied(changes.items(steps), report, true);
    if schema.result == SchemaResult::Applied {
        let mut summary = changes.summary();
        if steps > 0 {
            summary.push_str(&format!(
                "; {steps} database {}",
                if steps == 1 { "change" } else { "changes" }
            ));
        }
        ui::step(format_args!("Schema updated ({summary})"));
    }
    schema
}

/// The banner's rows about what applied.
fn row_labels(applied: &[&str]) -> Vec<&'static str> {
    applied
        .iter()
        .flat_map(|what| match *what {
            "auth" | "name" => &["Embedded", "Sign in"][..],
            "data" => &["Entities"],
            "functions" => &["Functions"],
            "storage" => &["Storage"],
            "connectors" => &["Connectors"],
            _ => &[],
        })
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::Router;

    use super::*;
    use crate::errors::HostError;
    use crate::integrations::FunctionsSlot;

    const AUTH_ONLY: &str = "name: notes\nservices:\n  auth:\n    enabled: true\n";

    #[tokio::test]
    async fn a_reload_on_request_applies_the_changes_and_answers_their_events() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let Ok(node) = NodeRuntime::discover().await else {
            eprintln!("skipped: Node.js is not on PATH");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("rayfin").join("rayfin.yml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, AUTH_ONLY).unwrap();
        let project = Project::load(dir.path()).unwrap();
        let state = LocalState::load_or_create(&project).unwrap();
        let (model, model_stamp) = analysis::analyze(&project, &state, &node, false)
            .await
            .unwrap();
        let model = Arc::new(model);
        let db = Db::open(&state.db_path()).unwrap();
        let settings = integrations::auth_settings(&project, &state.data().app_id, true);
        let auth = Arc::new(
            AuthService::new(db.clone(), state.keys_dir(), settings)
                .await
                .unwrap(),
        );
        let broker =
            Arc::new(integrations::token_broker(&project, &state, None, None, None).unwrap());
        let listeners = rl_server::bind("127.0.0.1", 0).await.unwrap();
        let api_origin = format!(
            "http://127.0.0.1:{}",
            listeners[0].local_addr().unwrap().port()
        );
        let options = ServeOptions::default();
        let (reload, requests) = channel();
        let ctx = ServeContext {
            project: &project,
            state: &state,
            node: &node,
            model: &model,
            model_stamp: &model_stamp,
            db: &db,
            auth: &auth,
            broker: &broker,
            data_api: None,
            options: &options,
            host: "127.0.0.1",
            api_origin: &api_origin,
            frontend_port: None,
            reload: reload.clone(),
        };
        let router = RouterHandle::new(Router::new());
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(rl_server::serve(router.router(), listeners, async {
            let _ = stopped.await;
        }));
        let features = Features {
            connectors: Router::new(),
            connector_models: HashMap::new(),
            storage: None,
            functions: FunctionsSlot::default(),
            secrets: SecretValues::default(),
        };
        let serving = Serving {
            features,
            router,
            static_site: None,
            app_url: None,
        };
        let mut hooks = StopHooks::default();
        start(&ctx, serving, requests, &mut hooks);

        let unchanged = reload.reload(false).await.unwrap();
        assert_eq!(unchanged.schema["type"], "schema");
        assert_eq!(unchanged.schema["result"], "unchanged");
        assert_eq!(unchanged.config, None);

        let with_password = AUTH_ONLY.replace(
            "enabled: true\n",
            "enabled: true\n    password:\n      enabled: true\n",
        );
        std::fs::write(&config, with_password).unwrap();
        let applied = reload.reload(false).await.unwrap();
        let changed = applied.config.unwrap();
        assert_eq!(changed["type"], "config-changed");
        assert_eq!(changed["applied"], true);
        assert_eq!(changed["what"], serde_json::json!(["auth"]));
        assert!(auth.settings().password_enabled);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let runtime: Value = client
            .get(format!("{api_origin}{}", rl_auth::RUNTIME_SETTINGS_PATH))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            runtime["serviceSettings"]["auth"]["password"]["enabled"],
            true
        );

        std::fs::write(&config, "services: [\n").unwrap();
        let invalid = reload.reload(true).await.unwrap_err();
        assert_eq!(HostError::classify(&invalid).code, ErrorCode::ConfigInvalid);
        assert!(auth.settings().password_enabled);

        hooks.run(Duration::from_secs(5)).await;
        let stopped = reload.reload(false).await.unwrap_err();
        assert!(stopped.to_string().contains("stopping"), "{stopped}");
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
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
            paths: Arc::new(RwLock::new(Paths {
                config: at("rayfin/rayfin.yml"),
                local_config: at("rayfin-local.toml"),
                data_dir: at("rayfin/data"),
                storage_dir: at("rayfin/storage"),
                tsconfig: at("rayfin/tsconfig.json"),
                connectors_dir: at("rayfin/connectors"),
                secrets: at(".rayfin-local/secrets.json"),
                functions_dir: at("rayfin/functions"),
                settings: at("rayfin/functions/local.settings.json"),
            })),
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
        let renamed = EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::To));
        let cases = [
            ("rayfin/rayfin.yml", edit, Some(Change::Config)),
            ("rayfin-local.toml", edit, Some(Change::Config)),
            (".rayfin-local/secrets.json", renamed, Some(Change::Secrets)),
            (
                "rayfin/functions/local.settings.json",
                edit,
                Some(Change::Secrets),
            ),
            (
                "rayfin/functions",
                EventKind::Create(CreateKind::Folder),
                Some(Change::Functions),
            ),
            (
                "rayfin/functions/package.json",
                edit,
                Some(Change::Functions),
            ),
            (
                "rayfin/connectors/inventory/metadata.json",
                edit,
                Some(Change::Connectors { folder: false }),
            ),
            (
                "rayfin/connectors",
                EventKind::Create(CreateKind::Folder),
                Some(Change::Connectors { folder: true }),
            ),
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
            (".rayfin-local/db.sqlite-wal", edit, None),
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
