//! The typed model of `rayfin/rayfin.yml` (Rayfin CLI 1.36.2 / 1.37 `RayfinConfig`) and its
//! parser.
//!
//! Parsing follows the CLI's `parseRayfinYamlInterpolated`: `${VAR}` interpolation, then the
//! defaults of `applyDefaults` and the connector normalization of `normalizeConnectorsBlock`.
//! Unknown keys do not fail the parse; their dotted paths are collected in
//! [`RayfinConfig::unknown_keys`].

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};

use crate::ProjectError;
use crate::error::strip_position_suffix;
use crate::interpolate::{interpolate_document, key_name};

/// `rayfin/rayfin.yml`.
///
/// Every service block is present after parsing (a missing block is disabled), as in the CLI.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RayfinConfig {
    /// Project slug (the Fabric item identifier).
    #[serde(default, deserialize_with = "de::opt_scalar_string")]
    pub id: Option<String>,
    /// Display name.
    #[serde(default, deserialize_with = "de::opt_scalar_string")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "de::opt_scalar_string")]
    pub version: Option<String>,
    #[serde(default, deserialize_with = "de::null_as_default")]
    pub services: ServicesConfig,
    /// External data connectors (a top-level list; the legacy map keyed by name is accepted).
    #[serde(default, deserialize_with = "de::null_as_default")]
    pub connectors: Vec<ConnectorEntry>,
    /// Secrets the functions read through `ctx.Secrets` (names only; values are never in here).
    #[serde(default, deserialize_with = "de::null_as_default")]
    pub secrets: Vec<SecretEntry>,
    /// Deprecated `frontend:` block (the framework is detected now).
    #[serde(default)]
    pub frontend: Option<FrontendConfig>,
    /// Dotted paths of the keys that are not part of the Rayfin configuration and were ignored,
    /// for example `deployment` or `services.auth.foo` (`connectors[0].bar` inside lists).
    #[serde(skip)]
    pub unknown_keys: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicesConfig {
    #[serde(default, deserialize_with = "de::null_as_default")]
    pub auth: AuthConfig,
    #[serde(default, deserialize_with = "de::null_as_default")]
    pub data: DataConfig,
    #[serde(default, deserialize_with = "de::null_as_default")]
    pub storage: StorageConfig,
    #[serde(default, deserialize_with = "de::null_as_default")]
    pub static_hosting: StaticHostingConfig,
    #[serde(default, deserialize_with = "de::null_as_default")]
    pub functions: FunctionsConfig,
    /// Application telemetry (experimental).
    #[serde(default)]
    pub telemetry: Option<Toggle>,
    /// `services.connectors.enabled`: a deprecated no-op opt-in (connectors are always
    /// available). Not to be confused with the top-level [`RayfinConfig::connectors`] list.
    #[serde(default, rename = "connectors")]
    pub connectors_opt_in: Option<Toggle>,
}

/// A block whose only setting is `enabled`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toggle {
    #[serde(default)]
    pub enabled: bool,
}

/// `services.auth`. The defaults and limits are the Rayfin host's (`AuthSettings`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Access-token lifetime as written; see [`AuthConfig::access_token_lifetime_minutes`].
    pub expiry_in_minutes: Option<i64>,
    /// Static claims added to every token.
    #[serde(default, deserialize_with = "de::scalar_string_map")]
    pub custom_claims: BTreeMap<String, String>,
    /// Dynamic custom claims (experimental).
    pub dynamic_claims: Option<DynamicClaimsConfig>,
    #[serde(default, deserialize_with = "de::null_as_default")]
    pub scopes: Vec<String>,
    pub refresh_token: Option<RefreshTokenConfig>,
    pub password: Option<PasswordConfig>,
    pub email: Option<EmailConfig>,
    pub passwordless: Option<PasswordlessConfig>,
    /// Fabric brokered sign-in (Entra SSO through the Fabric portal).
    pub fabric: Option<FabricAuthConfig>,
    /// Origins and URIs auth callbacks may return to (magic links, the Fabric handoff).
    #[serde(default, deserialize_with = "de::null_as_default")]
    pub allowed_redirect_uris: Vec<String>,
}

impl AuthConfig {
    pub const DEFAULT_EXPIRY_MINUTES: u32 = 60;
    pub const MIN_EXPIRY_MINUTES: u32 = 15;
    pub const MAX_EXPIRY_MINUTES: u32 = 43_200;
    pub const DEFAULT_REFRESH_LIFETIME_DAYS: u32 = 90;
    pub const MIN_REFRESH_LIFETIME_DAYS: u32 = 1;
    pub const MAX_REFRESH_LIFETIME_DAYS: u32 = 365;

    /// `expiryInMinutes`: 60 by default, clamped to 15–43 200 like the host does.
    pub fn access_token_lifetime_minutes(&self) -> u32 {
        clamp(
            self.expiry_in_minutes,
            Self::DEFAULT_EXPIRY_MINUTES,
            Self::MIN_EXPIRY_MINUTES,
            Self::MAX_EXPIRY_MINUTES,
        )
    }

    /// `refreshToken.lifetimeInDays`: 90 by default, clamped to 1–365 like the host does.
    pub fn refresh_token_lifetime_days(&self) -> u32 {
        clamp(
            self.refresh_token.as_ref().and_then(|r| r.lifetime_in_days),
            Self::DEFAULT_REFRESH_LIFETIME_DAYS,
            Self::MIN_REFRESH_LIFETIME_DAYS,
            Self::MAX_REFRESH_LIFETIME_DAYS,
        )
    }

    /// `refreshToken.absoluteLifetimeInMinutes`: the maximum session age (at least one minute),
    /// or `None` (the default, `-1`) for a sliding session without a hard limit.
    pub fn refresh_token_absolute_lifetime_minutes(&self) -> Option<u32> {
        let minutes = self.refresh_token.as_ref()?.absolute_lifetime_in_minutes?;
        (minutes >= 0).then(|| u32::try_from(minutes.max(1)).unwrap_or(u32::MAX))
    }

    /// Email + password sign-in. The host enables it only when `password.enabled` is `true`.
    pub fn password_enabled(&self) -> bool {
        self.password.as_ref().is_some_and(|p| p.enabled)
    }

    /// Fabric brokered sign-in (`fabric.enabled`).
    pub fn fabric_enabled(&self) -> bool {
        self.fabric.as_ref().is_some_and(|f| f.enabled)
    }

    /// External Entra token exchange (`fabric.externalEntraExchange`, off by default).
    pub fn external_entra_exchange_enabled(&self) -> bool {
        self.fabric
            .as_ref()
            .is_some_and(|f| f.external_entra_exchange)
    }

    /// Magic-link sign-in (`passwordless.magicLink.enabled`).
    pub fn magic_link_enabled(&self) -> bool {
        self.passwordless
            .as_ref()
            .and_then(|p| p.magic_link.as_ref())
            .is_some_and(|m| m.enabled)
    }
}

fn clamp(value: Option<i64>, default: u32, min: u32, max: u32) -> u32 {
    value.map_or(default, |v| {
        u32::try_from(v.clamp(i64::from(min), i64::from(max))).unwrap_or(default)
    })
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicClaimsConfig {
    pub enabled: Option<bool>,
    /// Filled in by deployments from the generated claim contract; never authored.
    pub contract: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshTokenConfig {
    pub lifetime_in_days: Option<i64>,
    pub absolute_lifetime_in_minutes: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasswordConfig {
    #[serde(default)]
    pub enabled: bool,
}

/// `services.auth.email`. Unset values take the host defaults (provider `smtp`, sender
/// `Rayfin Platform`, verification links 24 h, password-reset links 30 min).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailConfig {
    pub enabled: Option<bool>,
    pub provider: Option<String>,
    pub sender_name: Option<String>,
    pub verification_token_expiration_hours: Option<i64>,
    pub password_reset_token_expiration_minutes: Option<i64>,
    pub smtp: Option<SmtpConfig>,
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmtpConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub sender_email: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub use_ssl: Option<bool>,
    pub use_start_tls: Option<bool>,
    pub web_port: Option<u16>,
}

impl std::fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("sender_email", &self.sender_email)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("use_ssl", &self.use_ssl)
            .field("use_start_tls", &self.use_start_tls)
            .field("web_port", &self.web_port)
            .finish()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordlessConfig {
    pub magic_link: Option<MagicLinkConfig>,
    pub sms_otp: Option<Toggle>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MagicLinkConfig {
    #[serde(default)]
    pub enabled: bool,
    pub expiry_minutes: Option<i64>,
}

impl MagicLinkConfig {
    /// `expiryMinutes`: 15 by default, clamped to 5–60 like the host does.
    pub fn expiry_minutes(&self) -> u32 {
        clamp(self.expiry_minutes, 15, 5, 60)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FabricAuthConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Callers holding a delegated Entra token may exchange it for a session.
    #[serde(default)]
    pub external_entra_exchange: bool,
}

/// `services.data`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataConfig {
    #[serde(default)]
    pub enabled: bool,
    /// See [`DataConfig::dialect`].
    pub dialect: Option<Dialect>,
    /// Whether GraphQL introspection is allowed (off by default).
    #[serde(default)]
    pub allow_introspection: bool,
    /// The data package, relative to the project root (default: the root).
    pub path: Option<String>,
    /// Run in the data package before the entities are compiled.
    pub build_command: Option<String>,
}

impl DataConfig {
    /// The dialect; `mssql` when unset, as in the CLI.
    pub fn dialect(&self) -> Dialect {
        self.dialect.unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Dialect {
    #[default]
    #[serde(rename = "mssql")]
    MsSql,
    #[serde(rename = "postgresql")]
    PostgreSql,
}

impl Dialect {
    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::MsSql => "mssql",
            Dialect::PostgreSql => "postgresql",
        }
    }
}

/// `services.storage`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The storage package, relative to the project root (default: the root).
    pub path: Option<String>,
}

/// `services.staticHosting`: the frontend.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticHostingConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The frontend package, relative to the project root (default: the root). The build
    /// command runs here.
    pub path: Option<String>,
    /// The frontend project inside `path` (default: `path` itself).
    pub root: Option<String>,
    /// See [`StaticHostingConfig::folder`].
    pub folder: Option<String>,
    pub build_command: Option<String>,
    /// See [`StaticHostingConfig::index_document`].
    pub index_document: Option<String>,
    /// Who can load the hosted app; unset until the developer chooses.
    pub asset_access: Option<AssetAccess>,
    pub embedded: Option<EmbeddedConfig>,
}

impl StaticHostingConfig {
    pub const DEFAULT_FOLDER: &str = "dist";
    pub const DEFAULT_INDEX_DOCUMENT: &str = "index.html";

    /// The build output folder, relative to the frontend project: `dist` by default.
    pub fn folder(&self) -> &str {
        non_blank(self.folder.as_deref()).unwrap_or(Self::DEFAULT_FOLDER)
    }

    /// The document served for `/` and SPA routes: `index.html` by default.
    pub fn index_document(&self) -> &str {
        non_blank(self.index_document.as_deref()).unwrap_or(Self::DEFAULT_INDEX_DOCUMENT)
    }
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetAccess {
    /// Anyone can load the app.
    Public,
    /// Only signed-in users with access to the item can load the app.
    Protected,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddedConfig {
    /// The app loads only inside the Fabric portal.
    pub only: Option<bool>,
}

/// `services.functions`: User Data Functions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Required (with type `application`) when functions are enabled.
    pub auth: Option<FunctionsAuthConfig>,
    /// The functions package, relative to the project root (default: `rayfin/functions`).
    pub path: Option<String>,
    /// Run in the functions package to build it (for example `npm run build`).
    pub build_command: Option<String>,
}

impl FunctionsConfig {
    pub const DEFAULT_PATH: &str = "rayfin/functions";
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionsAuthConfig {
    #[serde(rename = "type")]
    pub auth_type: Option<FunctionsAuthType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FunctionsAuthType {
    Application,
    Delegated,
}

/// One entry of the top-level `connectors:` list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorEntry {
    /// The name the app invokes it by (`/connector-invoke/<name>`), unique ignoring case.
    #[serde(deserialize_with = "de::scalar_string")]
    pub name: String,
    /// The connector kind, for example `fabric-semanticmodel` or `fabric-sqldatabase`.
    #[serde(rename = "type")]
    pub connector_type: String,
    /// Pinned adapter version (required for function-bridge connectors).
    #[serde(default, deserialize_with = "de::opt_scalar_string")]
    pub version: Option<String>,
    /// Connector-specific settings (`workspaceId` and `itemId` for Fabric items).
    #[serde(default, deserialize_with = "de::null_as_default")]
    pub config: BTreeMap<String, serde_json::Value>,
    pub auth: Option<ConnectorAuth>,
    /// The operations the app may call.
    #[serde(default, deserialize_with = "de::null_as_default")]
    pub operations: Vec<ConnectorOperation>,
}

impl ConnectorEntry {
    /// `config.workspaceId`.
    pub fn workspace_id(&self) -> Option<&str> {
        self.config_str("workspaceId")
    }

    /// `config.itemId`.
    pub fn item_id(&self) -> Option<&str> {
        self.config_str("itemId")
    }

    /// A string value of `config`.
    pub fn config_str(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(serde_json::Value::as_str)
    }

    /// `auth.type`, when set.
    pub fn auth_type(&self) -> Option<ConnectorAuthType> {
        self.auth.as_ref().and_then(|auth| auth.auth_type)
    }

    /// Whether `operation` is declared (or `all` is).
    pub fn allows_operation(&self, operation: &str) -> bool {
        self.operations
            .iter()
            .any(|op| op.name == operation || op.name == "all")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorAuth {
    #[serde(rename = "type")]
    pub auth_type: Option<ConnectorAuthType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorAuthType {
    /// On behalf of the signed-in user.
    Delegated,
    /// As the app.
    Application,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorOperation {
    /// For example `executeQuery` or `read`.
    pub name: String,
}

/// One entry of the top-level `secrets:` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretEntry {
    #[serde(deserialize_with = "de::scalar_string")]
    pub name: String,
    pub description: Option<String>,
}

/// The deprecated `frontend:` block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontendConfig {
    pub framework: Option<String>,
    pub output_dir: Option<String>,
}

impl RayfinConfig {
    /// Reads and parses `rayfin.yml` at `path`; see [`RayfinConfig::parse`].
    pub fn load(
        path: &Path,
        vars: &BTreeMap<String, String>,
        env_file: &Path,
    ) -> Result<Self, ProjectError> {
        let source = std::fs::read_to_string(path).map_err(|source| ProjectError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&source, vars, path, env_file)
    }

    /// Parses `rayfin.yml` content. `${VAR}` references are resolved from `vars` (the env file
    /// overlaid with the process environment); `path` and `env_file` are used in messages.
    pub fn parse(
        source: &str,
        vars: &BTreeMap<String, String>,
        path: &Path,
        env_file: &Path,
    ) -> Result<Self, ProjectError> {
        let source = source.strip_prefix('\u{feff}').unwrap_or(source);
        let invalid = |line, column, message: String| ProjectError::InvalidConfig {
            path: path.to_path_buf(),
            line,
            column,
            message,
        };
        let document: Value = serde_yaml::from_str(source).map_err(|err| {
            let (line, column) = position(&err);
            invalid(line, column, without_position(&err))
        })?;
        let mut document = match document {
            Value::Mapping(_) => interpolate_document(document, vars, env_file)?,
            Value::Null => {
                return Err(invalid(None, None, "the file is empty".into()));
            }
            _ => {
                return Err(invalid(
                    Some(1),
                    Some(1),
                    "expected a mapping with id, name, version and services".into(),
                ));
            }
        };
        if let Value::Mapping(mapping) = &mut document {
            normalize_connectors(mapping);
        }

        let mut config: RayfinConfig = match serde_yaml::from_value(document.clone()) {
            Ok(config) => config,
            Err(err) => {
                let (message, line, column) = describe_schema_error(&err, &document, source);
                return Err(invalid(line, column, message));
            }
        };
        if let Ok(known) = serde_json::to_value(&config) {
            collect_unknown_keys(&document, &known, "", &mut config.unknown_keys);
        }
        Ok(config)
    }

    /// The connector the app invokes as `name`: an exact match, else the only entry whose name
    /// differs just by case (connector names are case-insensitive in Rayfin).
    pub fn connector(&self, name: &str) -> Option<&ConnectorEntry> {
        if let Some(exact) = self.connectors.iter().find(|c| c.name == name) {
            return Some(exact);
        }
        let mut matches = self
            .connectors
            .iter()
            .filter(|c| c.name.eq_ignore_ascii_case(name));
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }
}

/// `normalizeConnectorsBlock`: the legacy map (keyed by name) becomes a list, and the legacy
/// per-entry `connector:` field becomes `type:`.
fn normalize_connectors(document: &mut Mapping) {
    let Some(connectors) = document.get_mut("connectors") else {
        return;
    };
    let entries: Vec<Value> = match std::mem::take(connectors) {
        Value::Mapping(by_name) => by_name
            .into_iter()
            .map(|(name, value)| {
                let mut entry = Mapping::new();
                entry.insert(Value::String("name".into()), name);
                if let Value::Mapping(fields) = value {
                    entry.extend(fields);
                }
                Value::Mapping(entry)
            })
            .collect(),
        Value::Sequence(entries) => entries,
        other => {
            *connectors = other;
            return;
        }
    };
    *connectors = Value::Sequence(entries.into_iter().map(normalize_connector_entry).collect());
}

fn normalize_connector_entry(entry: Value) -> Value {
    let Value::Mapping(mut fields) = entry else {
        return entry;
    };
    let legacy_type = fields.remove("connector").filter(|v| !v.is_null());
    if fields.get("type").is_none_or(Value::is_null)
        && let Some(legacy_type) = legacy_type
    {
        fields.insert(Value::String("type".into()), legacy_type);
    }
    Value::Mapping(fields)
}

/// Collects the keys of `input` that the typed model dropped (`known` is the model serialized
/// back, so every known key is present there, even when unset).
fn collect_unknown_keys(
    input: &Value,
    known: &serde_json::Value,
    path: &str,
    out: &mut Vec<String>,
) {
    match (input, known) {
        (Value::Tagged(tagged), _) => collect_unknown_keys(&tagged.value, known, path, out),
        (Value::Mapping(mapping), serde_json::Value::Object(fields)) => {
            for (key, value) in mapping {
                let key = key_name(key);
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match fields.get(&key) {
                    Some(known) => collect_unknown_keys(value, known, &child, out),
                    None => out.push(child),
                }
            }
        }
        (Value::Sequence(items), serde_json::Value::Array(known_items)) => {
            for (index, (item, known)) in items.iter().zip(known_items).enumerate() {
                collect_unknown_keys(item, known, &format!("{path}[{index}]"), out);
            }
        }
        _ => {}
    }
}

/// `from_value` errors carry neither the setting's path nor a position. Deserializing the same
/// document from text yields the path; the original source yields the position when it fails
/// the same way (it may not, when the error comes from an interpolated value).
fn describe_schema_error(
    err: &serde_yaml::Error,
    document: &Value,
    source: &str,
) -> (String, Option<usize>, Option<usize>) {
    let message = serde_yaml::to_string(document)
        .ok()
        .and_then(|text| serde_yaml::from_str::<RayfinConfig>(&text).err())
        .map_or_else(|| err.to_string(), |err| without_position(&err));
    match serde_yaml::from_str::<RayfinConfig>(source) {
        Err(source_err) if without_position(&source_err) == message => {
            let (line, column) = position(&source_err);
            (message, line, column)
        }
        _ => (message, None, None),
    }
}

fn position(err: &serde_yaml::Error) -> (Option<usize>, Option<usize>) {
    err.location()
        .map_or((None, None), |l| (Some(l.line()), Some(l.column())))
}

fn without_position(err: &serde_yaml::Error) -> String {
    let message = err.to_string();
    match err.location() {
        Some(l) => strip_position_suffix(&message, l.line(), l.column()).to_string(),
        None => message,
    }
}

/// Lenient deserializers for values YAML may type differently than the CLI's JavaScript
/// consumers expect (`version: 1` is a number), and for `key:` with no value (null).
mod de {
    use std::collections::BTreeMap;
    use std::fmt;

    use serde::de::{Error, Unexpected, Visitor};
    use serde::{Deserialize, Deserializer};

    pub fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de> + Default,
    {
        Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
    }

    pub fn opt_scalar_string<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<String>, D::Error> {
        deserializer.deserialize_any(ScalarString)
    }

    pub fn scalar_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        opt_scalar_string(deserializer)?
            .ok_or_else(|| D::Error::invalid_type(Unexpected::Unit, &"a string"))
    }

    /// A string-to-string map whose values may be any scalar; null values are dropped.
    pub fn scalar_string_map<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, String>, D::Error> {
        let map = Option::<BTreeMap<String, Scalar>>::deserialize(deserializer)?;
        Ok(map
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(key, Scalar(value))| Some((key, value?)))
            .collect())
    }

    struct Scalar(Option<String>);

    impl<'de> Deserialize<'de> for Scalar {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_any(ScalarString).map(Scalar)
        }
    }

    struct ScalarString;

    impl<'de> Visitor<'de> for ScalarString {
        type Value = Option<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a string")
        }

        fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_string<E: Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(v))
        }

        fn visit_bool<E: Error>(self, v: bool) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_i64<E: Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_u64<E: Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_f64<E: Error>(self, v: f64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_none<E: Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_any(self)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Result<RayfinConfig, ProjectError> {
        parse_with(source, &[])
    }

    fn parse_with(source: &str, vars: &[(&str, &str)]) -> Result<RayfinConfig, ProjectError> {
        let vars = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        RayfinConfig::parse(
            source,
            &vars,
            Path::new("rayfin/rayfin.yml"),
            Path::new("rayfin/.env"),
        )
    }

    #[test]
    fn applies_the_cli_defaults() {
        let config = parse("id: app\nname: App\nversion: 1.0.0\n").unwrap();
        assert_eq!(config.id.as_deref(), Some("app"));
        let services = &config.services;
        assert!(!services.auth.enabled && !services.data.enabled && !services.storage.enabled);
        assert!(!services.static_hosting.enabled && !services.functions.enabled);
        assert_eq!(services.data.dialect(), Dialect::MsSql);
        assert_eq!(services.static_hosting.folder(), "dist");
        assert_eq!(services.static_hosting.index_document(), "index.html");
        assert_eq!(services.auth.access_token_lifetime_minutes(), 60);
        assert_eq!(services.auth.refresh_token_lifetime_days(), 90);
        assert_eq!(
            services.auth.refresh_token_absolute_lifetime_minutes(),
            None
        );
        assert!(!services.auth.password_enabled());
        assert!(config.connectors.is_empty() && config.unknown_keys.is_empty());
    }

    #[test]
    fn clamps_token_lifetimes_like_the_host() {
        let config = parse(
            "services:\n  auth:\n    expiryInMinutes: 5\n    refreshToken:\n      \
             lifetimeInDays: 1000\n      absoluteLifetimeInMinutes: 0\n",
        )
        .unwrap();
        assert_eq!(config.services.auth.access_token_lifetime_minutes(), 15);
        assert_eq!(config.services.auth.refresh_token_lifetime_days(), 365);
        assert_eq!(
            config
                .services
                .auth
                .refresh_token_absolute_lifetime_minutes(),
            Some(1)
        );
    }

    #[test]
    fn null_blocks_are_defaults() {
        let config = parse("services:\n  auth:\n  data: ~\nconnectors:\nsecrets:\n").unwrap();
        assert!(!config.services.auth.enabled);
        assert!(config.connectors.is_empty() && config.secrets.is_empty());
        assert!(config.unknown_keys.is_empty());
    }

    #[test]
    fn collects_unknown_keys_without_failing() {
        let config = parse(
            "id: x\npublishable_key: pk\ndeployment:\n  rayfinItemId: 1\nservices:\n  auth:\n    \
             enabled: true\n    issuer: x\n  connectors:\n    enabled: true\nconnectors:\n  - \
             name: a\n    type: kusto\n    extra: 1\n    config:\n      anything: goes\n",
        )
        .unwrap();
        assert_eq!(
            config.unknown_keys,
            [
                "publishable_key",
                "deployment",
                "services.auth.issuer",
                "connectors[0].extra"
            ]
        );
        assert_eq!(config.connectors[0].config_str("anything"), Some("goes"));
    }

    #[test]
    fn interpolates_and_coerces_values() {
        let config = parse_with(
            "services:\n  auth:\n    enabled: ${AUTH_ENABLED:-true}\n    expiryInMinutes: \
             ${EXPIRY}\n    allowedRedirectUris:\n      - http://localhost:${PORT}\n",
            &[("EXPIRY", "30"), ("PORT", "5175")],
        )
        .unwrap();
        assert!(config.services.auth.enabled);
        assert_eq!(config.services.auth.expiry_in_minutes, Some(30));
        assert_eq!(
            config.services.auth.allowed_redirect_uris,
            ["http://localhost:5175"]
        );
    }

    #[test]
    fn normalizes_legacy_connectors() {
        let config = parse(
            "connectors:\n  inventory:\n    connector: fabric-sqlanalytics\n    version: 1\n    \
             operations:\n      - name: read\n",
        )
        .unwrap();
        let connector = &config.connectors[0];
        assert_eq!(connector.name, "inventory");
        assert_eq!(connector.connector_type, "fabric-sqlanalytics");
        assert_eq!(connector.version.as_deref(), Some("1"));
        assert!(connector.allows_operation("read") && !connector.allows_operation("create"));
        assert!(config.unknown_keys.is_empty(), "{:?}", config.unknown_keys);
    }

    #[test]
    fn finds_connectors_ignoring_case_unless_ambiguous() {
        let config = parse(
            "connectors:\n  - {name: Sales, type: kusto}\n  - {name: orders, type: kusto}\n  - \
             {name: ORDERS, type: kusto}\n",
        )
        .unwrap();
        assert_eq!(
            config.connector("sales").map(|c| c.name.as_str()),
            Some("Sales")
        );
        assert_eq!(
            config.connector("ORDERS").map(|c| c.name.as_str()),
            Some("ORDERS")
        );
        assert!(config.connector("Orders").is_none());
    }

    #[test]
    fn reports_type_errors_with_path_and_position() {
        let err = parse("id: x\nservices:\n  data:\n    enabled: true\n    dialect: mysql\n")
            .unwrap_err();
        let message = err.to_string();
        assert!(
            message.starts_with("Invalid rayfin/rayfin.yml (line 5, column 14): "),
            "{message}"
        );
        assert!(
            message.contains("services.data.dialect: unknown variant `mysql`"),
            "{message}"
        );
    }

    #[test]
    fn reports_interpolated_type_errors_with_the_path() {
        let err = parse_with(
            "services:\n  auth:\n    enabled: ${ON}\n",
            &[("ON", "sometimes")],
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.starts_with("Invalid rayfin/rayfin.yml: services.auth.enabled:"),
            "{message}"
        );
    }

    #[test]
    fn reports_syntax_errors_with_position() {
        let err = parse("id: x\nservices:\n  auth: [\n").unwrap_err();
        assert!(
            matches!(err, ProjectError::InvalidConfig { line: Some(_), .. }),
            "{err}"
        );
        assert!(parse("").is_err());
        assert!(parse("- a\n- b\n").is_err());
    }

    #[test]
    fn debug_output_hides_the_smtp_password() {
        let config = parse(
            "services:\n  auth:\n    email:\n      smtp:\n        username: me\n        \
             password: hunter2\n",
        )
        .unwrap();
        let debug = format!("{config:?}");
        assert!(
            debug.contains("username: Some(\"me\")") && !debug.contains("hunter2"),
            "{debug}"
        );
    }

    #[test]
    fn missing_variables_fail() {
        let err = parse(
            "connectors:\n  - name: x\n    type: kusto\n    config:\n      itemId: \
                         ${ITEM}\n",
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Environment variable 'ITEM' referenced in rayfin.yml (connectors[0].config.itemId) \
             is not defined. Set it in rayfin/.env or in the shell environment."
        );
    }
}
