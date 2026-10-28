//! Live reload while serving: when the entity or storage sources or `rayfin.yml` change, the app
//! is analyzed again, the database brought up to date and the data API switched to the new model.
//! A failed analysis or a change that would lose data leaves the previous model in use.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use rl_db::Db;
use rl_graphql::DataApi;
use rl_model::{AppModel, Entity, Storage};
use rl_node::NodeRuntime;
use rl_project::{LocalState, Project, RayfinConfig};
use tokio::sync::mpsc;

use crate::analysis;
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
    let mut watcher =
        notify::recommended_watcher(move |result: notify::Result<notify::Event>| match result {
            Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                for path in event.paths {
                    if let Some(change) = filter.classify(&path) {
                        let _ = sender.send(change);
                    }
                }
            }
            Ok(_) => {}
            Err(err) => tracing::debug!("watching the project: {err}"),
        })
        .context("Could not watch the project's files")?;
    let rayfin_dir = ctx.project.rayfin_dir();
    let mut watched = vec![(rayfin_dir.as_path(), RecursiveMode::NonRecursive)];
    for dir in [&sources.data_dir, &sources.storage_dir] {
        if dir.is_dir() {
            watched.push((dir.as_path(), RecursiveMode::Recursive));
        }
    }
    for (path, mode) in watched {
        watcher
            .watch(path, mode)
            .with_context(|| format!("Could not watch {}", path.display()))?;
    }

    let reloader = Reloader {
        project: ctx.project.clone(),
        state: ctx.state.clone(),
        node: ctx.node.clone(),
        db: ctx.db.clone(),
        data_api: ctx.data_api.clone(),
        storage: ctx.model.storage.clone(),
        verbose: ctx.options.verbose,
    };
    let task = tokio::spawn(reloader.run(watcher, receiver));
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
    /// An entity or storage source.
    Model,
}

/// The files live reload follows.
#[derive(Clone)]
struct Sources {
    config: PathBuf,
    data_dir: PathBuf,
    storage_dir: PathBuf,
    /// The `tsconfig.json` next to the data folder, which the analysis compiles with.
    tsconfig: Option<PathBuf>,
}

impl Sources {
    fn of(project: &Project) -> Self {
        let data_dir = project.data_dir();
        Self {
            config: project.config_path(),
            tsconfig: data_dir.parent().map(|dir| dir.join("tsconfig.json")),
            data_dir,
            storage_dir: project.storage_schema_dir(),
        }
    }

    fn classify(&self, path: &Path) -> Option<Change> {
        if path == self.config {
            Some(Change::Config)
        } else if path.starts_with(&self.data_dir)
            || path.starts_with(&self.storage_dir)
            || self.tsconfig.as_deref() == Some(path)
        {
            Some(Change::Model)
        } else {
            None
        }
    }
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
    verbose: bool,
}

impl Reloader {
    async fn run(
        mut self,
        _watcher: RecommendedWatcher,
        mut changes: mpsc::UnboundedReceiver<Change>,
    ) {
        while let Some(first) = changes.recv().await {
            let mut batch = vec![first];
            while let Ok(Some(change)) = tokio::time::timeout(DEBOUNCE, changes.recv()).await {
                batch.push(change);
            }
            let mut analyze = batch.contains(&Change::Model);
            if batch.contains(&Change::Config) {
                analyze |= self.reload_config();
            }
            if analyze {
                self.reload_model().await;
            }
        }
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

    async fn reload_model(&mut self) {
        let services = &self.project.config().services;
        if !services.data.enabled && !services.storage.enabled {
            return;
        }
        let model =
            match analysis::reanalyze(&self.project, &self.state, &self.node, self.verbose).await {
                Ok(model) => model,
                Err(err) => {
                    ui::error(&err);
                    return;
                }
            };
        if model.storage != self.storage {
            self.storage = model.storage.clone();
            ui::warn("The storage folders changed — restart rayfin-local to apply them.");
        }
        let Some(data_api) = &self.data_api else {
            return;
        };
        let served = data_api.model();
        if served.entities == model.entities && served.dialect == model.dialect {
            return;
        }
        match rl_db::apply(&self.db, &model, false).await {
            Ok(report) => {
                for warning in &report.warnings {
                    ui::warn(warning);
                }
                let mut summary = describe_change(&served, &model);
                let steps = report.steps.len();
                if steps > 0 {
                    summary.push_str(&format!(
                        "; {steps} database {}",
                        if steps == 1 { "change" } else { "changes" }
                    ));
                }
                data_api.set_model(Arc::new(model));
                ui::step(format_args!("Schema updated ({summary})"));
            }
            Err(err) => {
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

/// The entities added, removed and changed, such as `added Tag; changed Todo`.
fn describe_change(served: &AppModel, new: &AppModel) -> String {
    fn by_name(model: &AppModel) -> BTreeMap<&str, &Entity> {
        model
            .entities
            .iter()
            .map(|entity| (entity.name.as_str(), entity))
            .collect()
    }
    let (served_entities, new_entities) = (by_name(served), by_name(new));
    let added: Vec<&str> = new_entities
        .keys()
        .filter(|name| !served_entities.contains_key(*name))
        .copied()
        .collect();
    let removed: Vec<&str> = served_entities
        .keys()
        .filter(|name| !new_entities.contains_key(*name))
        .copied()
        .collect();
    let changed: Vec<&str> = new_entities
        .iter()
        .filter(|(name, entity)| served_entities.get(*name).is_some_and(|old| old != *entity))
        .map(|(name, _)| *name)
        .collect();
    let mut parts = Vec::new();
    for (verb, names) in [("added", added), ("removed", removed), ("changed", changed)] {
        if !names.is_empty() {
            parts.push(format!("{verb} {}", names.join(", ")));
        }
    }
    if parts.is_empty() {
        parts.push(format!("dialect {}", new.dialect.as_str()));
    }
    parts.join("; ")
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
}
