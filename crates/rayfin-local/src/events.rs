//! The host contract's events (`serve --json`, docs/embedding.md): one JSON object per line on
//! stdout, and the live state a host reads back (the last `ready`, `schema`, `functions` and
//! `vite` events).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use rl_db::{MigrationReport, MigrationStep, SchemaError};
use rl_model::{AppModel, Entity};
use rl_project::Project;
use serde::Serialize;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use tracing::field::{Field, Visit};
use tracing::{Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// The version of the host contract.
pub const CONTRACT_VERSION: u32 = 1;

const TIME_FORMAT: &[BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// An event, as hosts read it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Event {
    Starting(Starting),
    Analysis(Analysis),
    Schema(Schema),
    Functions(Functions),
    Vite(Vite),
    Ready(Box<Ready>),
    ConfigChanged(ConfigChanged),
    Log(Log),
    Warning(Warning),
    Error(ErrorEvent),
    Stopped(Stopped),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Starting {
    pub pid: u32,
    pub version: String,
    pub project: ProjectInfo,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProjectInfo {
    pub root: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Analysis {
    pub ok: bool,
    pub sdk: Sdk,
    pub entities: Vec<String>,
    pub storage_folders: Vec<String>,
    pub message: Option<String>,
}

/// The app's Rayfin SDK: the installed version, and whether rayfin-local implements it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Sdk {
    pub version: Option<String>,
    pub supported: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Schema {
    pub result: SchemaResult,
    pub changes: Vec<String>,
    pub losses: Vec<String>,
    pub message: Option<String>,
    pub live: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SchemaResult {
    Unchanged,
    Applied,
    Refused,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Functions {
    pub state: FunctionsStatus,
    pub names: Vec<String>,
    pub errors: Vec<String>,
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FunctionsStatus {
    Building,
    Ready,
    Failed,
    Off,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Vite {
    pub state: ViteState,
    pub url: Option<String>,
    pub version: Option<String>,
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViteState {
    Ready,
    Exited,
    Restarting,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Ready {
    pub app_url: Option<String>,
    pub api_url: String,
    pub embed_url: Option<String>,
    pub portal_url: String,
    pub data_dir: String,
    pub sign_in_as: SignInAs,
    pub sdk: Sdk,
    pub features: Features,
}

/// Who the app is signed in as automatically: `{"kind": "developer"}`,
/// `{"kind": "user", "username": "…"}` or `{"kind": "none"}`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SignInAs {
    Developer,
    User { username: String },
    None,
}

impl SignInAs {
    pub fn of(identity: &rl_auth::SignInAs) -> Self {
        match identity {
            rl_auth::SignInAs::Developer => Self::Developer,
            rl_auth::SignInAs::User(username) => Self::User {
                username: username.clone(),
            },
            rl_auth::SignInAs::None => Self::None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Features {
    pub data: DataFeature,
    pub functions: FunctionsFeature,
    pub storage: StorageFeature,
    pub auth: AuthFeature,
    pub connectors: Vec<ConnectorFeature>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DataFeature {
    pub enabled: bool,
    pub entities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FunctionsFeature {
    pub enabled: bool,
    pub names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StorageFeature {
    pub enabled: bool,
    pub folders: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AuthFeature {
    pub enabled: bool,
    pub fabric: bool,
    pub password: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConnectorFeature {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub supported: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChanged {
    pub applied: bool,
    pub restart_required: bool,
    pub what: Vec<String>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Log {
    pub source: LogSource,
    pub level: LogLevel,
    pub line: String,
}

/// Who wrote a `log` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogSource {
    Vite,
    /// The function host and the functions' build.
    Functions,
    /// The data package's build (and the `--static` build).
    Build,
    RayfinLocal,
}

impl LogSource {
    /// The source of a child process's output, by its label (`[vite]`, `[fn]`, `[build]`).
    pub fn of_label(label: &str) -> Self {
        match label {
            "vite" => Self::Vite,
            "fn" => Self::Functions,
            "build" => Self::Build,
            _ => Self::RayfinLocal,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Warning {
    pub code: WarningCode,
    pub message: String,
    pub hint: Option<String>,
    pub details: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ErrorEvent {
    pub code: ErrorCode,
    pub message: String,
    pub hint: Option<String>,
    pub fatal: bool,
    pub details: Value,
}

/// The stable codes of `error` events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    NodeMissing,
    NodeTooOld,
    ProjectNotFound,
    ConfigInvalid,
    PackagesMissing,
    AnalysisFailed,
    SchemaRefused,
    SchemaFailed,
    PortInUse,
    AlreadyServing,
    ViteFailed,
    Internal,
}

/// The stable codes of `warning` events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WarningCode {
    SdkUnsupported,
    AnonymousRole,
    SecretMissing,
    ConnectorUnsupported,
    FunctionsBuildFailed,
    AzureSignIn,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stopped {
    pub reason: StopReason,
    pub exit_code: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopReason {
    StdinClosed,
    Signal,
    Error,
    BackendExited,
}

/// `{}`: the `details` of a code without any.
pub fn no_details() -> Value {
    Value::Object(Map::new())
}

impl Sdk {
    /// The project's installed Rayfin SDK.
    pub fn of(project: &Project) -> Self {
        let version = project.installed_sdk_version().map(str::to_owned);
        Self {
            supported: version
                .as_deref()
                .is_some_and(rl_project::is_supported_sdk_version),
            version,
        }
    }
}

impl Analysis {
    /// What an analysis of `project` came to.
    pub fn of(project: &Project, analyzed: Result<&AppModel, &anyhow::Error>) -> Self {
        let sdk = Sdk::of(project);
        match analyzed {
            Ok(model) => Self {
                ok: true,
                sdk,
                entities: model
                    .entities
                    .iter()
                    .map(|entity| entity.name.clone())
                    .collect(),
                storage_folders: model
                    .storage
                    .iter()
                    .flat_map(|storage| &storage.folders)
                    .map(|folder| folder.name.clone())
                    .collect(),
                message: None,
            },
            Err(err) => Self {
                ok: false,
                sdk,
                entities: Vec::new(),
                storage_folders: Vec::new(),
                message: Some(crate::ui::error_lines(err).join("\n")),
            },
        }
    }
}

impl Schema {
    /// Nothing to apply.
    pub fn unchanged(live: bool) -> Self {
        Self {
            result: SchemaResult::Unchanged,
            changes: Vec::new(),
            losses: Vec::new(),
            message: None,
            live,
        }
    }

    /// A migration that ran: `changes` from [`EntityChanges::items`].
    pub fn applied(changes: Vec<String>, report: &MigrationReport, live: bool) -> Self {
        if changes.is_empty() {
            return Self::unchanged(live);
        }
        Self {
            result: SchemaResult::Applied,
            changes,
            losses: report.forced.iter().map(ToString::to_string).collect(),
            message: None,
            live,
        }
    }

    /// A migration that was refused (it would lose data) or failed.
    pub fn rejected(err: &SchemaError, live: bool) -> Self {
        let (result, losses) = match err {
            SchemaError::DataLoss { losses } => (
                SchemaResult::Refused,
                losses.iter().map(ToString::to_string).collect(),
            ),
            _ => (SchemaResult::Failed, Vec::new()),
        };
        Self {
            result,
            changes: Vec::new(),
            losses,
            message: Some(err.to_string()),
            live,
        }
    }

    /// The database could not be opened, or updated for another reason.
    pub fn failed(err: &anyhow::Error, live: bool) -> Self {
        Self {
            result: SchemaResult::Failed,
            changes: Vec::new(),
            losses: Vec::new(),
            message: Some(crate::ui::error_lines(err).join("\n")),
            live,
        }
    }
}

/// The entities a schema change added, removed and changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EntityChanges {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    /// The new dialect, when only the dialect changed.
    pub dialect: Option<String>,
}

impl EntityChanges {
    /// Between the model being served and a new one.
    pub fn between(served: &AppModel, new: &AppModel) -> Self {
        fn by_name(model: &AppModel) -> BTreeMap<&str, &Entity> {
            model
                .entities
                .iter()
                .map(|entity| (entity.name.as_str(), entity))
                .collect()
        }
        let (served_entities, new_entities) = (by_name(served), by_name(new));
        let mut changes = Self {
            added: new_entities
                .keys()
                .filter(|name| !served_entities.contains_key(*name))
                .map(|name| (*name).to_owned())
                .collect(),
            removed: served_entities
                .keys()
                .filter(|name| !new_entities.contains_key(*name))
                .map(|name| (*name).to_owned())
                .collect(),
            changed: new_entities
                .iter()
                .filter(|(name, entity)| {
                    served_entities.get(*name).is_some_and(|old| old != *entity)
                })
                .map(|(name, _)| (*name).to_owned())
                .collect(),
            dialect: None,
        };
        if changes.is_empty() {
            changes.dialect = Some(new.dialect.as_str().to_owned());
        }
        changes
    }

    /// From the steps of a migration to `model` (at startup, when no model was served before).
    pub fn of_steps(steps: &[MigrationStep], model: &AppModel) -> Self {
        let entity_of_table = |table: &str| {
            model
                .entities
                .iter()
                .find(|entity| entity.table.eq_ignore_ascii_case(table))
                .map_or_else(|| table.to_owned(), |entity| entity.name.clone())
        };
        let (mut added, mut removed, mut changed) =
            (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for step in steps {
            match step {
                MigrationStep::CreateTable(table) => added.insert(table.entity.clone()),
                MigrationStep::DropTable(table) => removed.insert(table.entity.clone()),
                MigrationStep::AddColumn { table, .. } => changed.insert(table.entity.clone()),
                MigrationStep::RebuildTable { to, .. } => changed.insert(to.entity.clone()),
                MigrationStep::CreateIndex { table, .. }
                | MigrationStep::DropIndex { table, .. } => changed.insert(entity_of_table(table)),
            };
        }
        let changed = changed
            .into_iter()
            .filter(|name| !added.contains(name))
            .collect();
        Self {
            added: added.into_iter().collect(),
            removed: removed.into_iter().collect(),
            changed,
            dialect: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// `added Tag, Note; changed Todo`, or `dialect mssql`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (verb, names) in self.groups() {
            if !names.is_empty() {
                parts.push(format!("{verb} {}", names.join(", ")));
            }
        }
        if let Some(dialect) = &self.dialect {
            parts.push(format!("dialect {dialect}"));
        }
        parts.join("; ")
    }

    /// The `changes` of a `schema` event: `added Tag` for each entity, then the number of
    /// database steps (`3 database changes`).
    pub fn items(&self, steps: usize) -> Vec<String> {
        let mut items: Vec<String> = self
            .groups()
            .into_iter()
            .flat_map(|(verb, names)| names.iter().map(move |name| format!("{verb} {name}")))
            .collect();
        items.extend(
            self.dialect
                .iter()
                .map(|dialect| format!("dialect {dialect}")),
        );
        if steps > 0 {
            items.push(format!(
                "{steps} database {}",
                if steps == 1 { "change" } else { "changes" }
            ));
        }
        items
    }

    fn groups(&self) -> [(&'static str, &[String]); 3] {
        [
            ("added", &self.added),
            ("removed", &self.removed),
            ("changed", &self.changed),
        ]
    }
}

impl Event {
    /// The event as its line has it: `v`, `type` and `time`, then its fields.
    pub fn to_json(&self, time: &str) -> Value {
        let fields = match serde_json::to_value(self) {
            Ok(Value::Object(fields)) => fields,
            _ => Map::new(),
        };
        let mut line = Map::new();
        line.insert("v".into(), CONTRACT_VERSION.into());
        line.insert(
            "type".into(),
            fields.get("type").cloned().unwrap_or(Value::Null),
        );
        line.insert("time".into(), time.into());
        line.extend(fields.into_iter().filter(|(key, _)| key != "type"));
        Value::Object(line)
    }
}

/// The current time as events carry it: RFC 3339 in UTC, with milliseconds.
pub fn now() -> String {
    timestamp(OffsetDateTime::now_utc())
}

/// `at` as events write times: RFC 3339 in UTC, with milliseconds.
pub fn timestamp(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(TIME_FORMAT)
        .unwrap_or_default()
}

/// Where events are written with `--json`: one line each, flushed, until `stopped`.
struct Output {
    state: Mutex<OutputState>,
}

struct OutputState {
    writer: Box<dyn Write + Send>,
    /// `stopped` was written: it is the last event.
    closed: bool,
}

impl Output {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            state: Mutex::new(OutputState {
                writer,
                closed: false,
            }),
        }
    }

    fn write(&self, line: &Value, last: bool) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        let mut text = line.to_string();
        text.push('\n');
        // A host that stopped reading has nothing to tell; the end of stdin stops `serve`.
        let _ = state
            .writer
            .write_all(text.as_bytes())
            .and_then(|()| state.writer.flush());
        state.closed = last;
    }
}

static OUTPUT: OnceLock<Output> = OnceLock::new();
static LIVE: OnceLock<LiveState> = OnceLock::new();

/// Writes events to stdout from now on (`--json`); call before anything is printed.
pub fn init_json() {
    let _ = OUTPUT.set(Output::new(Box::new(io::stdout())));
}

/// Whether events are written (`--json`) instead of the human output.
pub fn is_json() -> bool {
    OUTPUT.get().is_some()
}

/// Emits `event`: written with `--json`, and kept in the [`live`] state when it is a `ready`,
/// `schema`, `functions` or `vite` event.
pub fn emit(event: Event) {
    publish(&event, false);
}

/// [`emit`], returning the event's line, for answers that carry the event.
pub fn emit_line(event: Event) -> Value {
    let line = event.to_json(&now());
    record(&event, &line, false);
    line
}

/// Emits `stopped`, the last event: events emitted after it are dropped.
pub fn finish(stopped: Stopped) {
    publish(&Event::Stopped(stopped), true);
}

fn publish(event: &Event, last: bool) {
    if OUTPUT.get().is_some() || LiveState::keeps(event) {
        record(event, &event.to_json(&now()), last);
    }
}

fn record(event: &Event, line: &Value, last: bool) {
    if LiveState::keeps(event) {
        live().record(line);
    }
    if let Some(output) = OUTPUT.get() {
        output.write(line, last);
    }
}

/// A `log` event.
pub fn log(source: LogSource, level: LogLevel, line: impl Into<String>) {
    emit(Event::Log(Log {
        source,
        level,
        line: line.into(),
    }));
}

/// A `warning` event.
pub fn warning(
    code: WarningCode,
    message: impl Into<String>,
    hint: Option<String>,
    details: Value,
) {
    emit(Event::Warning(Warning {
        code,
        message: message.into(),
        hint,
        details,
    }));
}

/// The live state of this process's `serve`.
pub fn live() -> LiveState {
    LIVE.get_or_init(LiveState::default).clone()
}

/// The last `ready`, `schema`, `functions` and `vite` events, as emitted (with or without
/// `--json`): what the control API's `GET /status` reports. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct LiveState(Arc<Mutex<Snapshot>>);

/// The events [`LiveState`] keeps, as their lines have them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub ready: Option<Value>,
    pub schema: Option<Value>,
    pub functions: Option<Value>,
    pub vite: Option<Value>,
}

impl LiveState {
    fn keeps(event: &Event) -> bool {
        matches!(
            event,
            Event::Ready(_) | Event::Schema(_) | Event::Functions(_) | Event::Vite(_)
        )
    }

    pub(crate) fn record(&self, line: &Value) {
        let mut snapshot = lock(&self.0);
        let slot = match line["type"].as_str() {
            Some("ready") => &mut snapshot.ready,
            Some("schema") => &mut snapshot.schema,
            Some("functions") => &mut snapshot.functions,
            Some("vite") => &mut snapshot.vite,
            _ => return,
        };
        *slot = Some(line.clone());
    }

    pub fn snapshot(&self) -> Snapshot {
        lock(&self.0).clone()
    }

    /// `GET /status` of the control API: the `ready` payload (without `v`, `type` and `time`),
    /// `"state": "running"`, and the last `schema`, `functions` and `vite` events (`null` when
    /// there was none). `None` before `ready`.
    pub fn status(&self) -> Option<Value> {
        let snapshot = self.snapshot();
        let Some(Value::Object(ready)) = snapshot.ready else {
            return None;
        };
        let mut status: Map<String, Value> = ready
            .into_iter()
            .filter(|(key, _)| !matches!(key.as_str(), "v" | "type" | "time"))
            .collect();
        status.insert("state".into(), "running".into());
        for (key, event) in [
            ("schema", snapshot.schema),
            ("functions", snapshot.functions),
            ("vite", snapshot.vite),
        ] {
            status.insert(key.into(), event.unwrap_or(Value::Null));
        }
        Some(Value::Object(status))
    }
}

/// With `--json`: rayfin-local's own logs (tracing) as `log` events, source `rayfin-local`;
/// debug ones only with `--verbose`.
pub struct LogLayer {
    pub verbose: bool,
}

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let level = match *event.metadata().level() {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            Level::INFO => LogLevel::Info,
            _ if self.verbose => LogLevel::Debug,
            _ => return,
        };
        let mut text = FieldText::default();
        event.record(&mut text);
        log(LogSource::RayfinLocal, level, text.finish());
    }
}

/// A tracing event's fields as one line: the message, then `name=value` for the others.
#[derive(Default)]
struct FieldText {
    message: String,
    fields: String,
}

impl FieldText {
    fn finish(self) -> String {
        match (self.message.is_empty(), self.fields.is_empty()) {
            (_, true) => self.message,
            (true, false) => self.fields,
            (false, false) => format!("{} {}", self.message, self.fields),
        }
    }
}

impl Visit for FieldText {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_owned();
        } else {
            self.record_debug(field, &value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let name = field.name();
        if name == "message" {
            self.message = format!("{value:?}");
        } else if !name.starts_with("log.") {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{name}={value:?}");
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    const TIME: &str = "2026-10-30T03:12:09.512Z";

    fn line(event: Event) -> Value {
        event.to_json(TIME)
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn lines_start_with_the_version_type_and_time() {
        let stopped = Event::Stopped(Stopped {
            reason: StopReason::StdinClosed,
            exit_code: 0,
        });
        assert_eq!(
            line(stopped).to_string(),
            r#"{"v":1,"type":"stopped","time":"2026-10-30T03:12:09.512Z","reason":"stdin-closed","exitCode":0}"#
        );
        let time = now();
        assert!(
            time::OffsetDateTime::parse(&time, &time::format_description::well_known::Rfc3339)
                .is_ok(),
            "{time}"
        );
        assert_eq!((time.len(), &time[19..20], &time[23..]), (24, ".", "Z"));
    }

    #[test]
    fn events_serialize_as_the_contract_shows_them() {
        let starting = Event::Starting(Starting {
            pid: 4242,
            version: "0.2.0".into(),
            project: ProjectInfo {
                root: "C:\\apps\\notes".into(),
                name: "notes".into(),
            },
        });
        assert_eq!(
            line(starting),
            parse(
                r#"{"v":1,"type":"starting","time":"2026-10-30T03:12:09.512Z","pid":4242,"version":"0.2.0","project":{"root":"C:\\apps\\notes","name":"notes"}}"#
            )
        );
        let sdk = Sdk {
            version: Some("1.36.2".into()),
            supported: true,
        };
        let analysis = Event::Analysis(Analysis {
            ok: true,
            sdk: sdk.clone(),
            entities: vec!["Note".into(), "Tag".into()],
            storage_folders: vec![],
            message: None,
        });
        assert_eq!(
            line(analysis),
            parse(
                r#"{"v":1,"type":"analysis","time":"2026-10-30T03:12:09.512Z","ok":true,"sdk":{"version":"1.36.2","supported":true},"entities":["Note","Tag"],"storageFolders":[],"message":null}"#
            )
        );
        let schema = Event::Schema(Schema {
            result: SchemaResult::Applied,
            changes: vec!["added Tag".into(), "changed Note".into()],
            losses: vec![],
            message: None,
            live: false,
        });
        assert_eq!(
            line(schema),
            parse(
                r#"{"v":1,"type":"schema","time":"2026-10-30T03:12:09.512Z","result":"applied","changes":["added Tag","changed Note"],"losses":[],"message":null,"live":false}"#
            )
        );
        let functions = Event::Functions(Functions {
            state: FunctionsStatus::Ready,
            names: vec!["getBacklog".into(), "askAnalyst".into()],
            errors: vec![],
            message: None,
        });
        assert_eq!(
            line(functions),
            parse(
                r#"{"v":1,"type":"functions","time":"2026-10-30T03:12:09.512Z","state":"ready","names":["getBacklog","askAnalyst"],"errors":[],"message":null}"#
            )
        );
        let vite = Event::Vite(Vite {
            state: ViteState::Ready,
            url: Some("http://localhost:5173/".into()),
            version: Some("8.0.16".into()),
            message: None,
        });
        assert_eq!(
            line(vite),
            parse(
                r#"{"v":1,"type":"vite","time":"2026-10-30T03:12:09.512Z","state":"ready","url":"http://localhost:5173/","version":"8.0.16","message":null}"#
            )
        );
        let changed = Event::ConfigChanged(ConfigChanged {
            applied: true,
            restart_required: false,
            what: vec!["storage".into(), "functions".into()],
            message: None,
        });
        assert_eq!(
            line(changed),
            parse(
                r#"{"v":1,"type":"config-changed","time":"2026-10-30T03:12:09.512Z","applied":true,"restartRequired":false,"what":["storage","functions"],"message":null}"#
            )
        );
        let log = Event::Log(Log {
            source: LogSource::Vite,
            level: LogLevel::Info,
            line: "page reload src/App.tsx".into(),
        });
        assert_eq!(
            line(log),
            parse(
                r#"{"v":1,"type":"log","time":"2026-10-30T03:12:09.512Z","source":"vite","level":"info","line":"page reload src/App.tsx"}"#
            )
        );
        let error = Event::Error(ErrorEvent {
            code: ErrorCode::SchemaRefused,
            message: "The change would delete data: …".into(),
            hint: Some("Restart with --force-schema to apply it, or --reset to start over.".into()),
            fatal: true,
            details: json!({ "losses": ["…"] }),
        });
        assert_eq!(
            line(error),
            parse(
                r#"{"v":1,"type":"error","time":"2026-10-30T03:12:09.512Z","code":"schema-refused","message":"The change would delete data: …","hint":"Restart with --force-schema to apply it, or --reset to start over.","fatal":true,"details":{"losses":["…"]}}"#
            )
        );
        let warning = Event::Warning(Warning {
            code: WarningCode::SecretMissing,
            message: "secret API_KEY has no local value".into(),
            hint: None,
            details: json!({ "name": "API_KEY" }),
        });
        assert_eq!(
            line(warning),
            parse(
                r#"{"v":1,"type":"warning","time":"2026-10-30T03:12:09.512Z","code":"secret-missing","message":"secret API_KEY has no local value","hint":null,"details":{"name":"API_KEY"}}"#
            )
        );
        assert_eq!(
            line(Event::Ready(Box::new(ready()))),
            parse(
                r#"{
                  "v": 1, "type": "ready", "time": "2026-10-30T03:12:09.512Z",
                  "appUrl": "http://localhost:5173/",
                  "apiUrl": "http://localhost:7171",
                  "embedUrl": "http://localhost:7171/.rayfin-local/portal/embed",
                  "portalUrl": "http://localhost:7171/.rayfin-local/portal",
                  "dataDir": "C:\\apps\\notes\\.rayfin-local",
                  "signInAs": {"kind": "developer"},
                  "sdk": {"version": "1.36.2", "supported": true},
                  "features": {
                    "data": {"enabled": true, "entities": ["Note", "Tag"]},
                    "functions": {"enabled": true, "names": ["getBacklog"]},
                    "storage": {"enabled": false, "folders": []},
                    "auth": {"enabled": true, "fabric": true, "password": false},
                    "connectors": [
                      {"name": "sales", "type": "fabric-semanticmodel", "supported": true},
                      {"name": "logs", "type": "kusto", "supported": false}
                    ]
                  }
                }"#
            )
        );
        let user = SignInAs::of(&rl_auth::SignInAs::User("ana@example.com".into()));
        assert_eq!(
            serde_json::to_value(user).unwrap(),
            json!({ "kind": "user", "username": "ana@example.com" })
        );
        let nobody = SignInAs::of(&rl_auth::SignInAs::None);
        assert_eq!(
            serde_json::to_value(nobody).unwrap(),
            json!({ "kind": "none" })
        );
    }

    fn ready() -> Ready {
        Ready {
            app_url: Some("http://localhost:5173/".into()),
            api_url: "http://localhost:7171".into(),
            embed_url: Some("http://localhost:7171/.rayfin-local/portal/embed".into()),
            portal_url: "http://localhost:7171/.rayfin-local/portal".into(),
            data_dir: "C:\\apps\\notes\\.rayfin-local".into(),
            sign_in_as: SignInAs::Developer,
            sdk: Sdk {
                version: Some("1.36.2".into()),
                supported: true,
            },
            features: Features {
                data: DataFeature {
                    enabled: true,
                    entities: vec!["Note".into(), "Tag".into()],
                },
                functions: FunctionsFeature {
                    enabled: true,
                    names: vec!["getBacklog".into()],
                },
                storage: StorageFeature {
                    enabled: false,
                    folders: vec![],
                },
                auth: AuthFeature {
                    enabled: true,
                    fabric: true,
                    password: false,
                },
                connectors: vec![
                    ConnectorFeature {
                        name: "sales".into(),
                        kind: "fabric-semanticmodel".into(),
                        supported: true,
                    },
                    ConnectorFeature {
                        name: "logs".into(),
                        kind: "kusto".into(),
                        supported: false,
                    },
                ],
            },
        }
    }

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn each_event_is_a_line_and_none_follows_stopped() {
        let buffer = Buffer::default();
        let output = Output::new(Box::new(buffer.clone()));
        output.write(&json!({ "type": "log" }), false);
        output.write(&json!({ "type": "stopped" }), true);
        output.write(&json!({ "type": "log" }), false);
        let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text, "{\"type\":\"log\"}\n{\"type\":\"stopped\"}\n");
    }

    #[test]
    fn the_status_is_the_ready_payload_and_the_last_events() {
        let live = LiveState::default();
        let schema = |result| {
            Event::Schema(Schema {
                result,
                ..Schema::unchanged(true)
            })
        };
        live.record(&line(schema(SchemaResult::Unchanged)));
        assert_eq!(live.status(), None);
        live.record(&line(Event::Ready(Box::new(ready()))));
        live.record(&line(schema(SchemaResult::Refused)));
        live.record(&line(Event::Log(Log {
            source: LogSource::RayfinLocal,
            level: LogLevel::Info,
            line: "kept out".into(),
        })));
        let status = live.status().unwrap();
        assert_eq!(status["state"], "running");
        assert_eq!(status["apiUrl"], "http://localhost:7171");
        assert!(status.get("type").is_none() && status.get("time").is_none());
        assert_eq!(status["schema"]["result"], "refused");
        assert_eq!(status["functions"], Value::Null);
        assert_eq!(live.snapshot().vite, None);
    }

    fn model() -> AppModel {
        let analysis = include_str!("../../rl-model/tests/fixtures/basic.analysis.json");
        AppModel::from_analysis(&serde_json::from_str(analysis).unwrap()).unwrap()
    }

    #[test]
    fn schema_changes_name_the_entities() {
        let model = model();
        let plan = rl_db::plan(None, &model).unwrap();
        let first_run = EntityChanges::of_steps(&plan.steps, &model);
        let mut names: Vec<String> = model.entities.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(first_run.added, names);
        assert!(first_run.changed.is_empty() && first_run.removed.is_empty());

        let mut changed = model.clone();
        let removed = changed.entities.remove(0).name;
        changed.entities[0].columns.pop();
        let edited = changed.entities[0].name.clone();
        let changes = EntityChanges::between(&model, &changed);
        assert_eq!(
            changes.summary(),
            format!("removed {removed}; changed {edited}")
        );
        assert_eq!(
            changes.items(3),
            [
                format!("removed {removed}"),
                format!("changed {edited}"),
                "3 database changes".to_owned()
            ]
        );
        let same = EntityChanges::between(&model, &model);
        assert_eq!(
            same.summary(),
            format!("dialect {}", model.dialect.as_str())
        );
        assert_eq!(
            same.items(0),
            [format!("dialect {}", model.dialect.as_str())]
        );
    }

    /// Records what [`FieldText`] makes of each event.
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl<S: Subscriber> Layer<S> for Recorder {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut text = FieldText::default();
            event.record(&mut text);
            self.0.lock().unwrap().push(text.finish());
        }
    }

    #[test]
    fn log_lines_have_the_message_then_the_fields() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(Recorder(Arc::clone(&lines)));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("building the functions");
            tracing::warn!(port = 7171, path = %"C:\\app", "port in use");
            tracing::info!(label = "vite", pid = 12);
        });
        assert_eq!(
            *lines.lock().unwrap(),
            [
                "building the functions",
                "port in use port=7171 path=C:\\app",
                "label=\"vite\" pid=12",
            ]
        );
    }
}
