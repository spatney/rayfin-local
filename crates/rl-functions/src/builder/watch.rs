//! Watching the functions package: the compiled output and the sources.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
    process::ExitStatus,
    time::Duration,
};

use notify::{EventKind, RecursiveMode, Watcher, event::ModifyKind};
use rl_node::{ManagedChild, ProcessSpec};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

use super::{
    BUILD_WATCH_SCRIPT, BuildConfig, BuildError, LABEL, Package, build, normalize, output_dirs_of,
};
use crate::metadata::{is_ignored_source_dir, is_package_config, is_typescript};

/// Changes closer together than this make one event.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// The longest a change waits for its event while changes keep coming.
const MAX_DELAY: Duration = Duration::from_secs(2);

/// How long `npm run build:watch` gets to exit before its process tree is killed.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// A change in the functions package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEvent {
    /// The compiled output changed: the function host should restart, after
    /// [`metadata::ensure_fresh`](crate::metadata::ensure_fresh), since the UDF package reads
    /// `runtimemetadata.json` when it starts.
    OutputChanged,
    /// A TypeScript source, `package.json` or `tsconfig*.json` changed: `runtimemetadata.json`
    /// may be stale.
    SourceChanged,
    /// The build command, run again after source changes ([`WatchMode::Rebuild`]), finished:
    /// `ok` when it succeeded.
    Rebuilt { ok: bool },
    /// `npm run build:watch` exited (with this status, when known), so the output is no longer
    /// recompiled.
    BuildWatchExited(Option<ExitStatus>),
}

/// How the output is kept compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    /// The package's `build:watch` script runs.
    Script,
    /// The package has no `build:watch` script: the build command runs again when the sources
    /// change.
    Rebuild,
}

/// Watches the functions package. Dropping it stops everything, `npm run build:watch` included.
pub struct FunctionsWatch {
    events: mpsc::Receiver<WatchEvent>,
    mode: WatchMode,
    output_dirs: Vec<PathBuf>,
    _watcher: notify::RecommendedWatcher,
    tasks: Vec<JoinHandle<()>>,
    script: Option<(oneshot::Sender<()>, JoinHandle<()>)>,
}

impl FunctionsWatch {
    /// The next change. Bursts of changes make one event, about 300 ms after the last change
    /// (at most 2 s after the first).
    pub async fn next(&mut self) -> Option<WatchEvent> {
        self.events.recv().await
    }

    /// A change that is already waiting, if any.
    pub fn try_next(&mut self) -> Option<WatchEvent> {
        self.events.try_recv().ok()
    }

    pub fn mode(&self) -> WatchMode {
        self.mode
    }

    /// The folders watched for compiled output.
    pub fn output_dirs(&self) -> &[PathBuf] {
        &self.output_dirs
    }

    /// Stops watching and waits until `npm run build:watch`, if it runs, has exited.
    pub async fn stop(mut self) {
        if let Some((stop, supervisor)) = self.script.take() {
            let _ = stop.send(());
            let _ = supervisor.await;
        }
    }
}

impl Drop for FunctionsWatch {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        if let Some((stop, _supervisor)) = self.script.take() {
            let _ = stop.send(());
        }
    }
}

impl fmt::Debug for FunctionsWatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FunctionsWatch")
            .field("mode", &self.mode)
            .field("output_dirs", &self.output_dirs)
            .finish_non_exhaustive()
    }
}

/// Starts watching the functions package (after a [`build`](super::build)): `npm run
/// build:watch` when the package has that script, else the build command whenever the sources
/// change. Must be called within a Tokio runtime.
pub fn watch(config: &BuildConfig) -> Result<FunctionsWatch, BuildError> {
    let root = normalize(
        &std::path::absolute(&config.functions_dir)
            .unwrap_or_else(|_| config.functions_dir.clone()),
    );
    let package = Package::read(&root)?;
    let mode = if package.script(BUILD_WATCH_SCRIPT).is_some() {
        WatchMode::Script
    } else {
        WatchMode::Rebuild
    };
    let output_dirs = output_dirs_of(&root, &package)?;
    for dir in &output_dirs {
        fs::create_dir_all(dir).map_err(|source| BuildError::Watch {
            path: dir.clone(),
            source: notify::Error::io(source),
        })?;
    }
    let src = root.join("src");

    let (changes, changes_rx) = mpsc::unbounded_channel();
    let paths = WatchPaths::new(&root, &src, &output_dirs);
    let mut watcher =
        notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
            Ok(event) if is_change(&event.kind) => {
                for path in &event.paths {
                    let (output, source) = paths.classify(path);
                    if output {
                        let _ = changes.send(Change::Output);
                    }
                    if source {
                        let _ = changes.send(Change::Source);
                    }
                }
            }
            Ok(_) => {}
            Err(error) => tracing::warn!("watching the functions failed: {error}"),
        })
        .map_err(|source| BuildError::Watch {
            path: root.clone(),
            source,
        })?;
    let mut watched = Vec::new();
    for dir in &output_dirs {
        let mode = if *dir == root {
            RecursiveMode::NonRecursive
        } else {
            RecursiveMode::Recursive
        };
        add_watch(&mut watched, dir, mode);
    }
    if src.is_dir() {
        add_watch(&mut watched, &src, RecursiveMode::Recursive);
    }
    add_watch(&mut watched, &root, RecursiveMode::NonRecursive);
    for (dir, mode) in &watched {
        watcher
            .watch(dir, *mode)
            .map_err(|source| BuildError::Watch {
                path: dir.clone(),
                source,
            })?;
    }

    let (events_tx, events) = mpsc::channel(16);
    let child = match mode {
        WatchMode::Script => {
            let spec = ProcessSpec::shell(LABEL, format!("npm run {BUILD_WATCH_SCRIPT}"), &root)
                .sink(config.sink.clone());
            Some(
                ManagedChild::spawn(spec).map_err(|source| BuildError::StartWatch {
                    dir: root.clone(),
                    source,
                })?,
            )
        }
        WatchMode::Rebuild => None,
    };
    let mut tasks = Vec::new();
    let rebuild = (mode == WatchMode::Rebuild).then(|| {
        let (requests, requests_rx) = mpsc::unbounded_channel();
        let config = BuildConfig {
            functions_dir: root.clone(),
            ..config.clone()
        };
        tasks.push(tokio::spawn(rebuild_on_request(
            config,
            requests_rx,
            events_tx.clone(),
        )));
        requests
    });
    tasks.push(tokio::spawn(debounce(
        changes_rx,
        events_tx.clone(),
        rebuild,
    )));
    let script = child.map(|child| {
        let (stop, stop_rx) = oneshot::channel();
        let supervisor = tokio::spawn(supervise_script(child, stop_rx, events_tx));
        (stop, supervisor)
    });

    Ok(FunctionsWatch {
        events,
        mode,
        output_dirs,
        _watcher: watcher,
        tasks,
        script,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Output,
    Source,
}

fn add_watch(watched: &mut Vec<(PathBuf, RecursiveMode)>, dir: &Path, mode: RecursiveMode) {
    if !watched.iter().any(|(existing, _)| existing == dir) {
        watched.push((dir.to_owned(), mode));
    }
}

/// Content changes; reads (`Access`) and attribute updates are not.
fn is_change(kind: &EventKind) -> bool {
    !matches!(
        kind,
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_))
    )
}

/// The watched folders, each as configured and canonicalized (macOS reports real paths).
#[derive(Clone)]
struct WatchPaths {
    roots: Vec<PathBuf>,
    srcs: Vec<PathBuf>,
    /// Output folders, and whether their subfolders count.
    outputs: Vec<(PathBuf, bool)>,
}

impl WatchPaths {
    fn new(root: &Path, src: &Path, output_dirs: &[PathBuf]) -> Self {
        let forms = |path: &Path| {
            let mut forms = vec![path.to_owned()];
            if let Ok(canonical) = fs::canonicalize(path)
                && canonical != path
            {
                forms.push(canonical);
            }
            forms
        };
        Self {
            roots: forms(root),
            srcs: forms(src),
            outputs: output_dirs
                .iter()
                .flat_map(|dir| {
                    let recursive = dir != root;
                    forms(dir).into_iter().map(move |form| (form, recursive))
                })
                .collect(),
        }
    }

    /// Whether a changed path is compiled output, and whether it is an input of the metadata.
    fn classify(&self, path: &Path) -> (bool, bool) {
        // An output folder's own events are not output (see below), even once it is gone: the
        // files in it report their removal themselves.
        let in_output = self.outputs.iter().any(|(dir, recursive)| {
            path.strip_prefix(dir).is_ok_and(|relative| {
                !relative.as_os_str().is_empty()
                    && (*recursive || relative.components().count() <= 1)
                    && !relative
                        .components()
                        .any(|component| component.as_os_str() == "node_modules")
            })
        });
        let in_src = self.srcs.iter().any(|src| {
            path.strip_prefix(src).is_ok_and(|relative| {
                !relative.components().any(|component| {
                    is_ignored_source_dir(&component.as_os_str().to_string_lossy())
                })
            })
        });
        let typescript_source = in_src && is_typescript(path);
        // A folder's own events (its timestamp changes with its entries) are not output; the
        // files in it report their changes themselves.
        let output = in_output && !typescript_source && !is_ignored_output(path) && !path.is_dir();
        // Declarations emitted next to the sources are output, not sources.
        let source = (typescript_source && !(in_output && is_declaration(path)))
            || (path
                .parent()
                .is_some_and(|parent| self.roots.iter().any(|root| root == parent))
                && path
                    .file_name()
                    .is_some_and(|name| is_package_config(&name.to_string_lossy())));
        (output, source)
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_declaration(path: &Path) -> bool {
    let name = file_name(path);
    [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// Build artifacts that do not change what runs: build info, source maps and declarations.
fn is_ignored_output(path: &Path) -> bool {
    let name = file_name(path);
    name.ends_with(".tsbuildinfo") || name.ends_with(".map") || is_declaration(path)
}

/// Changes of one kind not yet reported.
#[derive(Clone, Copy)]
struct Pending {
    first: Instant,
    last: Instant,
}

impl Pending {
    /// When to report: after [`DEBOUNCE`] without changes, or [`MAX_DELAY`] after the first.
    fn due(self) -> Instant {
        (self.last + DEBOUNCE).min(self.first + MAX_DELAY)
    }

    fn record(pending: &mut Option<Pending>) {
        let now = Instant::now();
        match pending {
            Some(pending) => pending.last = now,
            None => {
                *pending = Some(Pending {
                    first: now,
                    last: now,
                })
            }
        }
    }

    fn take_due(pending: &mut Option<Pending>, now: Instant) -> bool {
        let due = pending.is_some_and(|pending| pending.due() <= now);
        if due {
            *pending = None;
        }
        due
    }
}

/// Turns bursts of changes into one event per kind (see [`Pending::due`]); source changes also
/// request a rebuild when `rebuild` is set.
async fn debounce(
    mut changes: mpsc::UnboundedReceiver<Change>,
    events: mpsc::Sender<WatchEvent>,
    rebuild: Option<mpsc::UnboundedSender<()>>,
) {
    let mut output: Option<Pending> = None;
    let mut source: Option<Pending> = None;
    loop {
        let due = output.into_iter().chain(source).map(Pending::due).min();
        tokio::select! {
            change = changes.recv() => match change {
                Some(Change::Output) => Pending::record(&mut output),
                Some(Change::Source) => Pending::record(&mut source),
                None => return,
            },
            () = sleep_until(due) => {
                let now = Instant::now();
                if Pending::take_due(&mut source, now) {
                    // A full channel already holds this notification.
                    let _ = events.try_send(WatchEvent::SourceChanged);
                    if let Some(rebuild) = &rebuild {
                        let _ = rebuild.send(());
                    }
                }
                if Pending::take_due(&mut output, now) {
                    let _ = events.try_send(WatchEvent::OutputChanged);
                }
            }
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Runs the build once per batch of requests, never two at a time, and reports each outcome.
async fn rebuild_on_request(
    config: BuildConfig,
    mut requests: mpsc::UnboundedReceiver<()>,
    events: mpsc::Sender<WatchEvent>,
) {
    while requests.recv().await.is_some() {
        while requests.try_recv().is_ok() {}
        let ok = match build(&config).await {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!("{error}");
                false
            }
        };
        let _ = events.send(WatchEvent::Rebuilt { ok }).await;
    }
}

/// Reports `npm run build:watch` exiting, or stops it when `stop` fires (or is dropped).
async fn supervise_script(
    mut child: ManagedChild,
    stop: oneshot::Receiver<()>,
    events: mpsc::Sender<WatchEvent>,
) {
    tokio::select! {
        status = child.wait() => {
            let status = status.ok();
            let how = status.map(|status| format!(" ({status})")).unwrap_or_default();
            tracing::warn!(
                "`npm run {BUILD_WATCH_SCRIPT}` exited{how}: the functions are no longer \
                 recompiled when they change"
            );
            let _ = events.try_send(WatchEvent::BuildWatchExited(status));
        }
        _ = stop => {
            if let Err(error) = child.kill(STOP_GRACE).await {
                tracing::debug!("stopping `npm run {BUILD_WATCH_SCRIPT}`: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::timeout;

    use super::*;

    #[test]
    fn changes_are_classified() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(root.join("dist").join("src")).unwrap();
        let classify = |paths: &WatchPaths, relative: &str| paths.classify(&root.join(relative));

        let paths = WatchPaths::new(root, &src, &[root.join("dist")]);
        for (relative, expected) in [
            ("dist/src/function_app.js", (true, false)),
            ("dist/src", (false, false)),
            ("dist/gone", (true, false)),
            ("dist/src/function_app.js.map", (false, false)),
            ("dist/src/function_app.d.ts", (false, false)),
            ("dist/tsconfig.tsbuildinfo", (false, false)),
            ("src/function_app.ts", (false, true)),
            ("src/nested/extra.mts", (false, true)),
            ("src/notes.md", (false, false)),
            ("src/node_modules/x/index.ts", (false, false)),
            ("package.json", (false, true)),
            ("tsconfig.build.json", (false, true)),
            ("runtimemetadata.json", (false, false)),
            ("local.settings.json", (false, false)),
            ("node_modules/x/package.json", (false, false)),
        ] {
            assert_eq!(classify(&paths, relative), expected, "{relative}");
        }

        // JavaScript emitted next to the sources.
        let in_place = WatchPaths::new(root, &src, std::slice::from_ref(&src));
        assert_eq!(classify(&in_place, "src/app.js"), (true, false));
        assert_eq!(classify(&in_place, "src/app.d.ts"), (false, false));
        assert_eq!(classify(&in_place, "src/app.ts"), (false, true));

        // An output folder's own event, also once the folder is gone.
        let removed = WatchPaths::new(root, &src, &[root.join("out")]);
        assert_eq!(classify(&removed, "out"), (false, false));
        assert_eq!(classify(&removed, "out/index.js"), (true, false));

        // `main` at the package root: only its own folder is output.
        let flat = WatchPaths::new(root, &src, &[root.to_owned()]);
        assert_eq!(classify(&flat, "index.js"), (true, false));
        assert_eq!(classify(&flat, "lib/helper.js"), (false, false));
    }

    #[tokio::test]
    async fn bursts_become_one_event_per_kind() {
        let (changes, changes_rx) = mpsc::unbounded_channel();
        let (events_tx, mut events) = mpsc::channel(16);
        let (rebuild, mut rebuilds) = mpsc::unbounded_channel();
        let task = tokio::spawn(debounce(changes_rx, events_tx, Some(rebuild)));
        for _ in 0..5 {
            changes.send(Change::Output).unwrap();
            changes.send(Change::Source).unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let mut received = Vec::new();
        for _ in 0..2 {
            let event = timeout(Duration::from_secs(5), events.recv()).await;
            received.push(event.unwrap().unwrap());
        }
        assert!(
            received.contains(&WatchEvent::OutputChanged),
            "{received:?}"
        );
        assert!(
            received.contains(&WatchEvent::SourceChanged),
            "{received:?}"
        );
        assert!(
            timeout(DEBOUNCE * 2, events.recv()).await.is_err(),
            "one event per burst"
        );
        assert_eq!(rebuilds.try_recv(), Ok(()));
        assert!(rebuilds.try_recv().is_err());

        drop(changes);
        timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn steady_changes_are_still_reported() {
        let (changes, changes_rx) = mpsc::unbounded_channel();
        let (events_tx, mut events) = mpsc::channel(16);
        let _task = tokio::spawn(debounce(changes_rx, events_tx, None));
        let started = Instant::now();
        let writer = tokio::spawn(async move {
            while changes.send(Change::Output).is_ok() {
                tokio::time::sleep(DEBOUNCE / 3).await;
            }
        });
        let event = timeout(MAX_DELAY * 2, events.recv()).await.unwrap();
        assert_eq!(event, Some(WatchEvent::OutputChanged));
        assert!(started.elapsed() >= MAX_DELAY);
        writer.abort();
    }
}
