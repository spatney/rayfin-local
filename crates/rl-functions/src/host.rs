//! The Node function host: the app's own functions package, run on the real
//! `@microsoft/fabric-user-data-functions` and `@azure/functions` packages without Azure Functions
//! Core Tools ([`FunctionHost`], supervised by [`HostManager`]).
//!
//! # How the 1.36.2 packages work (sources: the installed dists)
//!
//! - **Registration.** `new UserDataFunctions()` reads `runtimemetadata.json` from
//!   `join(dirname(resolve('./src')), 'runtimemetadata.json')`, which is
//!   `<cwd>/runtimemetadata.json`: the host's working directory must be the functions package.
//!   The file must be schema `2.0` with only the keys `functionName`, `delegateParameters` and
//!   `contextAudiences` per function (1.37's `capabilities` is rejected). When it is missing or
//!   invalid nothing throws: every function still registers, and every call answers
//!   `Failed`/`InternalError` "Unable to determine function metadata…" with `{reason}`.
//! - `udf.func(name, fn)` calls `app.http(name, {methods: ['POST'], authLevel: 'anonymous',
//!   extraInputs})`. `@azure/functions` then calls `registerFunction` on `@azure/functions-core`
//!   with bindings `httpTrigger<hash>` (the trigger), `$return` (`http`, out) and, per audience
//!   in the function's `contextAudiences` (plus generic connections passed in the legacy third
//!   argument), a `FabricItem` input `{audienceType, argName: '__generic_<Audience>'}`, with an
//!   `audienceScope` override for `AzureAI` (`https://ai.azure.com/user_impersonation`) and `ADO`.
//!   Binding names are hashes of the binding JSON, so the host reports them
//!   ([`RegisteredFunction::inputs`]).
//! - **`@azure/functions-core`** is not on npm: the Azure Functions Node.js worker injects it.
//!   `@azure/functions` `require`s it lazily, inside a try/catch, on the first registration, and
//!   calls `setProgrammingModel`, `registerFunction`, `registerHook` (`app.hook.*`) and `log`.
//!   Without it, the library switches to "test mode" and registers nothing.
//! - **Arguments.** The handler parses `await request.json()` (anything unparsable is `{}`) and
//!   takes each `delegateParameters[i]` by name from it: `string` also accepts numbers and
//!   booleans, `number` numeric strings; booleans, arrays and `object` are strict; other types
//!   pass through. A parameter absent from the body without a default is `MissingInput`.
//! - **The context** (a parameter whose type text starts with `RayfinContext`) comes from
//!   `RayfinContext.fromRequest`:
//!   - With `AZURE_FUNCTIONS_ENVIRONMENT=Development` (which `rayfin dev` writes into
//!     `local.settings.json`), the endpoint is `RAYFIN_API_URL`, the key
//!     `RAYFIN_PUBLISHABLE_KEY`, and the token the request's `Authorization: Bearer`, else
//!     `RAYFIN_ACCESS_TOKEN`. `RAYFIN_DELEGATED_{STORAGE,SQL,AI}_TOKEN` fill missing `Storage`,
//!     `Sql` and `AI` tokens (sic: `AI`, not `AzureAI`).
//!   - Otherwise (deployed), the `x-ms-rayfin-info` header carries
//!     `{"rayFinEndpoint", "publishableKey", "rayfinToken"}` ([`rayfin_info_header`]). A trailing
//!     `/graphql` is stripped from the endpoint.
//!   - `ctx.Secrets.<NAME>` is the `x-ms-rayfin-secrets` header's JSON object of strings
//!     ([`secrets_header`]), else `process.env.<NAME>`. (`x-ms-rayfin-secrets-available` is
//!     1.37 only.) Header values must be byte strings: `new Request()` throws on code points above
//!     0xFF, before any envelope exists.
//!   - `ctx.Tokens.<Audience>` reads the FabricItem input's value,
//!     `{"Endpoints": {"<Audience>": {"AccessToken": "<token>"}}}`
//!     ([`RegisteredFunction::token_bindings`]). A declared audience without one throws
//!     "No token available for audience…" (`Failed`/`InvalidInput`) when read.
//! - **Response.** `{status, body: <envelope>, headers: {'Content-Type': 'application/json',
//!   'x-fabric-udf-status': <status>}}`, which `@azure/functions` serializes with lowercase header
//!   names. The envelope is `{functionName, invocationId, status, output, errors: [{errorCode,
//!   message, properties}]}`, status `Succeeded`, `BadRequest`, `Failed` or `ResponseTooLarge`
//!   (over 30 MiB). The HTTP status is 200 unless `IsLocal=true` (also written by `rayfin dev`):
//!   then 400 for `BadRequest` (422 for `UserThrown`), 500 for `Failed`, 403 for
//!   `ResponseTooLarge`. A return value `JSON.stringify` can't handle (a `BigInt`) throws outside
//!   the UDF try/catch: there is no envelope, and the invocation fails ([`HostError::Failed`]).
//! - **Logs.** The UDF package replaces `console.*`; inside an invocation each call goes to the
//!   invocation's `context.log`, prefixed `[UserDataFunctions]`.
//!
//! # What callers supply
//!
//! - `env`: the package's `local.settings.json` `Values` ([`local_settings_values`]), as
//!   `func start` sets them, but with `RAYFIN_API_URL` and `RAYFIN_PUBLISHABLE_KEY` pointing at
//!   rayfin-local (in Development mode functions would otherwise call the deployed backend).
//!   Keep or drop `IsLocal` to choose the status codes; [`HostResponse::udf_status`] is there
//!   either way. `languageWorkers__node__arguments` (e.g. `--inspect=9229`, which `rayfin dev`
//!   writes for debugging) means nothing to Node.js: map it to [`HostConfig::inspect_port`] or
//!   [`HostConfig::node_args`] if wanted.
//! - Per invocation: the caller's `Authorization` header *and* `x-ms-rayfin-info` (so the
//!   context is right in either mode), `x-ms-rayfin-secrets`, and token bindings for
//!   [`RegisteredFunction::audiences`].
//!
//! # The host process (`js/function-host.mjs`)
//!
//! `node [--enable-source-maps] [--inspect=127.0.0.1:<port>] function-host.mjs --entry <main>`,
//! with the functions package as its working directory and the caller's env on top of this
//! process's. It provides `@azure/functions-core` to `require` (as the worker does), imports the
//! entry point, runs `appStart` hooks and announces itself on stdout with one short line (the
//! output reader splits lines beyond 64 KiB):
//!
//! - `@@rayfin-local:ready {"port"}`, or
//! - `@@rayfin-local:load-error {"name", "message", "stack"}`, after which it exits 1.
//!
//! Every other line is the app's output and goes to the sink, labelled `fn`. Requests go over
//! TCP to `127.0.0.1:<port>`, one connection per request and one JSON line each way, carrying the
//! secret from `RAYFIN_LOCAL_FUNCTION_HOST_SECRET` (which the host removes from its env):
//!
//! - `{"type": "describe", secret}` → `{"ok": true, "programmingModel", "functions":
//!   [RegisteredFunction]}`, right after `ready`;
//! - `{"type": "invoke", secret, invocationId, functionName, method, url, headers, body,
//!   bodyEncoding, bindings}` → `{"ok": true, "response": {status, headers: [[name, value]], body,
//!   bodyEncoding}}` or `{"ok": false, "error": {kind, name, message, stack}}`, where the kind
//!   `shuttingDown` ([`HostError::ShuttingDown`]) refuses invocations that arrive after a
//!   shutdown;
//! - `{"type": "shutdown", secret, drainMs}` → `{"ok": true}`; the host lets in-flight
//!   invocations deliver their replies (up to `drainMs`; a later shutdown can only shorten that),
//!   runs `appTerminate` hooks and exits 0, as soon as no invocation is in flight.
//!
//! Bodies are UTF-8 text, or `latin1` (one char per byte) when they are not valid UTF-8. A plain
//! TCP exchange needs no HTTP client (reqwest would need a process-wide rustls provider even for
//! `http://`), keeps invocations independent, and a crash shows up as a closed connection.
//!
//! The host runs each invocation as the worker does: `getInvocationModel(coreCtx)`,
//! `getArguments()`, `preInvocation` hooks, `invokeFunction`, `postInvocation` hooks and
//! `getResponse`, so the `HttpRequest`, `InvocationContext` and response come from the app's own
//! `@azure/functions`.
//!
//! [`FunctionHost`]: crate::host::FunctionHost
//! [`HostManager`]: crate::host::HostManager
//! [`HostError::Failed`]: crate::host::HostError::Failed
//! [`HostError::ShuttingDown`]: crate::host::HostError::ShuttingDown
//! [`HostResponse::udf_status`]: crate::host::HostResponse::udf_status
//! [`HostConfig::inspect_port`]: crate::host::HostConfig::inspect_port
//! [`HostConfig::node_args`]: crate::host::HostConfig::node_args
//! [`RegisteredFunction::inputs`]: crate::host::RegisteredFunction::inputs
//! [`RegisteredFunction::audiences`]: crate::host::RegisteredFunction::audiences
//! [`RegisteredFunction::token_bindings`]: crate::host::RegisteredFunction::token_bindings
//! [`local_settings_values`]: crate::host::local_settings_values
//! [`rayfin_info_header`]: crate::host::rayfin_info_header
//! [`secrets_header`]: crate::host::secrets_header

mod manager;
mod process;
mod wire;

use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use rl_node::{LineSink, NodeError, NodeRuntime};
use rl_tokens::Audience;
use serde::Deserialize;
use serde_json::{Map, Value};

pub use manager::{HostManager, HostStatus, ManagerOptions};
pub use process::{FunctionHost, HostExit};

/// The embedded host script.
const HOST_SCRIPT: rl_node::Helper = rl_node::Helper::new(
    "function-host",
    include_str!("../../../js/function-host.mjs"),
);

/// The environment variable that passes the invoke socket's secret to the host.
const SECRET_ENV: &str = "RAYFIN_LOCAL_FUNCTION_HOST_SECRET";

/// What [`FunctionHost::start`] needs.
#[derive(Clone)]
pub struct HostConfig {
    pub node: NodeRuntime,
    /// The functions package: the host's working directory, where `runtimemetadata.json` is read.
    pub functions_dir: PathBuf,
    /// The package's `main`, absolute or relative to `functions_dir`.
    pub entry: PathBuf,
    /// Set on top of this process's environment, in order: `local.settings.json` `Values` and
    /// the caller's overrides (see the module docs).
    pub env: Vec<(String, String)>,
    /// Starts the Node.js inspector on `127.0.0.1:<port>`.
    pub inspect_port: Option<u16>,
    /// Extra Node.js options, before the script (e.g. `--max-old-space-size=512`).
    pub node_args: Vec<String>,
    /// Where the host script is materialized (`<project>/.rayfin-local/cache`).
    pub cache_dir: PathBuf,
    /// Receives the host's output (the app's `console.*` and `context.log`), labelled `fn`.
    pub sink: LineSink,
    /// How long loading the package may take.
    pub ready_timeout: Duration,
}

impl HostConfig {
    /// A config with no extra env, no inspector and a 60 s ready timeout.
    pub fn new(
        node: NodeRuntime,
        functions_dir: impl Into<PathBuf>,
        entry: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        sink: LineSink,
    ) -> Self {
        Self {
            node,
            functions_dir: functions_dir.into(),
            entry: entry.into(),
            env: Vec::new(),
            inspect_port: None,
            node_args: Vec::new(),
            cache_dir: cache_dir.into(),
            sink,
            ready_timeout: Duration::from_secs(60),
        }
    }

    /// The entry point as an absolute path.
    pub fn entry_path(&self) -> PathBuf {
        self.functions_dir.join(&self.entry)
    }
}

impl fmt::Debug for HostConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Env values may be secrets (local.settings.json holds them): names only.
        f.debug_struct("HostConfig")
            .field("node", &self.node)
            .field("functions_dir", &self.functions_dir)
            .field("entry", &self.entry)
            .field("env", &self.env.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .field("inspect_port", &self.inspect_port)
            .field("node_args", &self.node_args)
            .field("cache_dir", &self.cache_dir)
            .field("ready_timeout", &self.ready_timeout)
            .finish_non_exhaustive()
    }
}

/// A function the package registered, as `@azure/functions` described it to the worker.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisteredFunction {
    pub name: String,
    /// The trigger binding's type: `httpTrigger` for every `udf.func`.
    pub trigger: Option<String>,
    /// Upper-case HTTP methods (`["POST"]` for `udf.func`).
    #[serde(default)]
    pub methods: Vec<String>,
    /// The HTTP route template, when the function set one.
    pub route: Option<String>,
    pub auth_level: Option<String>,
    /// The extra input bindings (`FabricItem` connections).
    #[serde(default)]
    pub inputs: Vec<InputBinding>,
}

/// An extra input binding of a registered function.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputBinding {
    /// The binding's name: the key of its value in [`HostInvocation::bindings`].
    pub name: String,
    #[serde(rename = "type")]
    pub binding_type: String,
    /// `FabricItem` generic connections: the `AudienceType` member (`ADO`, `AzureAI`, …).
    pub audience_type: Option<String>,
    /// The scope the UDF package asks the host to use for the audience, when it overrides it.
    pub audience_scope: Option<String>,
    /// `FabricItem` alias connections: the managed item's alias.
    pub alias: Option<String>,
    pub arg_name: Option<String>,
}

impl RegisteredFunction {
    /// Whether this is an HTTP function, which [`FunctionHost::invoke`] can run.
    pub fn is_http(&self) -> bool {
        self.trigger
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("httpTrigger"))
    }

    /// Whether the function accepts `method` (an HTTP function without methods accepts all).
    pub fn accepts(&self, method: &str) -> bool {
        self.is_http()
            && (self.methods.is_empty()
                || self.methods.iter().any(|m| m.eq_ignore_ascii_case(method)))
    }

    /// The generic `FabricItem` inputs: one per audience whose token `ctx.Tokens` exposes.
    pub fn audience_inputs(&self) -> impl Iterator<Item = &InputBinding> {
        self.inputs.iter().filter(|input| {
            input.binding_type.eq_ignore_ascii_case("FabricItem") && input.audience_type.is_some()
        })
    }

    /// The audiences the function declared, in binding order (names rl-tokens doesn't know are
    /// left out).
    pub fn audiences(&self) -> Vec<Audience> {
        let mut audiences = Vec::new();
        for input in self.audience_inputs() {
            if let Some(audience) = input
                .audience_type
                .as_deref()
                .and_then(Audience::from_udf_name)
                && !audiences.contains(&audience)
            {
                audiences.push(audience);
            }
        }
        audiences
    }

    /// The [`HostInvocation::bindings`] that deliver `tokens` to `ctx.Tokens`: for each generic
    /// `FabricItem` input whose audience has a token, the binding data the Fabric host extension
    /// sends, `{"Endpoints": {"<Audience>": {"AccessToken": "<token>"}}}`. Audiences without a
    /// token get no binding, so reading them throws in the function, as in Fabric.
    pub fn token_bindings<'a>(
        &self,
        tokens: impl IntoIterator<Item = (Audience, &'a str)>,
    ) -> Map<String, Value> {
        let tokens: HashMap<Audience, &str> = tokens.into_iter().collect();
        let mut bindings = Map::new();
        for input in self.audience_inputs() {
            let Some(audience_type) = input.audience_type.as_deref() else {
                continue;
            };
            let Some(token) = Audience::from_udf_name(audience_type).and_then(|a| tokens.get(&a))
            else {
                continue;
            };
            let mut endpoint = Map::new();
            endpoint.insert("AccessToken".into(), Value::from(*token));
            let mut endpoints = Map::new();
            endpoints.insert(audience_type.into(), Value::Object(endpoint));
            let mut data = Map::new();
            data.insert("Endpoints".into(), Value::Object(endpoints));
            bindings.insert(input.name.clone(), Value::Object(data));
        }
        bindings
    }
}

/// One HTTP request to a registered function.
#[derive(Clone)]
pub struct HostInvocation {
    pub function_name: String,
    /// The invocation id (`context.invocationId`, the envelope's `invocationId`).
    pub invocation_id: String,
    pub method: String,
    /// The request URL the function sees; defaults to `http://localhost/api/<route or name>`.
    pub url: Option<String>,
    /// Request headers. Values must be byte strings (code points up to 0xFF), as in Azure.
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// Values for the function's extra input bindings, by binding name (see
    /// [`RegisteredFunction::token_bindings`]).
    pub bindings: Map<String, Value>,
}

impl HostInvocation {
    /// A `POST` with a new invocation id.
    pub fn new(function_name: impl Into<String>, body: impl Into<Bytes>) -> Self {
        Self {
            function_name: function_name.into(),
            invocation_id: uuid::Uuid::new_v4().to_string(),
            method: "POST".into(),
            url: None,
            headers: Vec::new(),
            body: body.into(),
            bindings: Map::new(),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

impl fmt::Debug for HostInvocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Headers carry bearer tokens and secrets, bindings carry tokens: names only.
        f.debug_struct("HostInvocation")
            .field("function_name", &self.function_name)
            .field("invocation_id", &self.invocation_id)
            .field("method", &self.method)
            .field("url", &self.url)
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("body_len", &self.body.len())
            .field("bindings", &self.bindings.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// The HTTP response the Functions host would send: for `udf.func`, the UDF envelope, unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostResponse {
    pub status: u16,
    /// Lower-case names, in the order `@azure/functions` produced them.
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl HostResponse {
    /// The first value of header `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The envelope status (`Succeeded`, `BadRequest`, `Failed`, `ResponseTooLarge`) from
    /// `x-fabric-udf-status`; `None` for functions that aren't UDFs.
    pub fn udf_status(&self) -> Option<&str> {
        self.header("x-fabric-udf-status")
    }
}

/// Why the host could not run a function.
///
/// For the invoke routes: `Starting`, `ShuttingDown` and `Stopped` are 503, `NotFound` and
/// `NotInvocable` 404, `Timeout` 408. The others mean no envelope exists; `Failed` carries what
/// the UDF package puts in an `InternalError` (`error_type`, `message`), and the start failures
/// (`MissingEntry`, `Node`, `Load`, `ExitedBeforeReady`, `StartTimeout`) say what to fix in the
/// package; those that may pass ([`is_transient`](Self::is_transient)) are 503 with `Retry-After`.
#[derive(Clone, Debug, thiserror::Error)]
pub enum HostError {
    #[error(
        "the functions entry point {} does not exist; build the functions package first",
        .entry.display()
    )]
    MissingEntry { entry: PathBuf },
    #[error("could not start the function host: {0}")]
    Node(Arc<NodeError>),
    /// The entry point threw while loading; `message` and `stack` are the JavaScript error's.
    #[error("the functions package failed to load ({entry}): {message}")]
    Load {
        entry: String,
        message: String,
        stack: Option<String>,
    },
    #[error("the function host exited before it was ready ({exit}); see its output (`fn`)")]
    ExitedBeforeReady { exit: String },
    #[error("the functions package did not finish loading within {0:?}")]
    StartTimeout(Duration),
    /// The host is (re)starting; answer 503.
    #[error("the function host is starting; try again in a moment")]
    Starting,
    /// The host was asked to shut down (it was replaced, or is stopping) and refused the
    /// invocation without running it; [`HostManager`] runs it on the current host instead.
    #[error("the function host is shutting down; try again in a moment")]
    ShuttingDown,
    #[error("the function host is stopped")]
    Stopped,
    #[error("no function named '{0}' is registered")]
    NotFound(String),
    #[error("function '{name}' has a {trigger} trigger; only HTTP functions can be invoked")]
    NotInvocable { name: String, trigger: String },
    /// The invocation threw instead of producing a response (e.g. a `BigInt` result).
    /// `error_type` is the JavaScript error's name.
    #[error("function '{function}' failed without a response: {error_type}: {message}")]
    Failed {
        function: String,
        error_type: String,
        message: String,
        stack: Option<String>,
    },
    #[error("function '{function}' did not finish within {timeout:?}")]
    Timeout { function: String, timeout: Duration },
    /// The host process ended while the function ran (a crash, `process.exit`, a restart).
    #[error("the function host exited while '{function}' was running ({exit})")]
    Exited { function: String, exit: String },
    #[error("function host protocol error: {0}")]
    Protocol(String),
    #[error("could not reach the function host: {0}")]
    Io(Arc<std::io::Error>),
}

impl From<NodeError> for HostError {
    fn from(error: NodeError) -> Self {
        HostError::Node(Arc::new(error))
    }
}

impl HostError {
    /// Whether a failed start may succeed when tried again: the package took too long to load,
    /// the process ended without a word (e.g. the inspector's port was still taken), or the new
    /// host could not be reached. A package that threw while loading stays failed until a reload.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            HostError::StartTimeout(_) | HostError::ExitedBeforeReady { .. } | HostError::Io(_)
        )
    }
}

impl From<std::io::Error> for HostError {
    fn from(error: std::io::Error) -> Self {
        HostError::Io(Arc::new(error))
    }
}

/// The `Values` of `<functions_dir>/local.settings.json`, which `func start` sets as environment
/// variables. Empty when there is no such file; values that aren't strings become their JSON text.
pub fn local_settings_values(functions_dir: &Path) -> std::io::Result<Vec<(String, String)>> {
    let path = functions_dir.join("local.settings.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let invalid = |message: String| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {message}", path.display()),
        )
    };
    let settings: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| invalid(format!("not valid JSON ({e})")))?;
    if settings["IsEncrypted"] == Value::Bool(true) {
        return Err(invalid(
            "the values are encrypted; run `func settings decrypt` first".into(),
        ));
    }
    let Some(values) = settings.get("Values") else {
        return Ok(Vec::new());
    };
    let values = values
        .as_object()
        .ok_or_else(|| invalid("`Values` is not an object".into()))?;
    Ok(values
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| {
            let value = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            (key.clone(), value)
        })
        .collect())
}

/// The `x-ms-rayfin-info` header value: what a deployed function's context is built from
/// (`rayfinToken` is `""` for anonymous callers).
pub fn rayfin_info_header(
    api_url: &str,
    publishable_key: &str,
    rayfin_token: Option<&str>,
) -> String {
    let mut info = Map::new();
    info.insert("rayFinEndpoint".into(), Value::from(api_url));
    info.insert("publishableKey".into(), Value::from(publishable_key));
    info.insert(
        "rayfinToken".into(),
        Value::from(rayfin_token.unwrap_or("")),
    );
    ascii_json(&Value::Object(info))
}

/// The `x-ms-rayfin-secrets` header value: a JSON object of secret names to values, with
/// non-ASCII characters escaped so the value is a valid header.
pub fn secrets_header<'a>(secrets: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let bag: Map<String, Value> = secrets
        .into_iter()
        .map(|(name, value)| (name.to_owned(), Value::from(value)))
        .collect();
    ascii_json(&Value::Object(bag))
}

/// Serializes `value` as JSON with every non-ASCII character as a `\u` escape (they can only
/// occur inside strings, where an escape means the same thing).
fn ascii_json(value: &Value) -> String {
    let json = value.to_string();
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens_function() -> RegisteredFunction {
        serde_json::from_value(serde_json::json!({
            "name": "tokens",
            "trigger": "httpTrigger",
            "methods": ["POST"],
            "route": null,
            "authLevel": "anonymous",
            "inputs": [
                {"name": "FabricItemInput1", "type": "FabricItem", "audienceType": "ADO",
                 "audienceScope": "499b84ac-1321-427f-aa17-267ca6975798/.default",
                 "argName": "__generic_ADO"},
                {"name": "FabricItemInput2", "type": "FabricItem", "audienceType": "AzureAI",
                 "argName": "__generic_AzureAI"},
                {"name": "FabricItemInput3", "type": "FabricItem", "alias": "lake",
                 "argName": "lake"},
                {"name": "FabricItemInput4", "type": "FabricItem", "audienceType": "Kusto",
                 "argName": "__generic_Kusto"}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn token_bindings_carry_each_available_token_under_its_binding() {
        let function = tokens_function();
        assert_eq!(function.audiences(), vec![Audience::Ado, Audience::AzureAi]);

        let bindings =
            function.token_bindings([(Audience::AzureAi, "ai-token"), (Audience::Fabric, "x")]);
        assert_eq!(
            Value::Object(bindings),
            serde_json::json!({
                "FabricItemInput2": {"Endpoints": {"AzureAI": {"AccessToken": "ai-token"}}}
            })
        );
    }

    #[test]
    fn functions_accept_their_methods_only() {
        let function = tokens_function();
        assert!(function.accepts("post"));
        assert!(!function.accepts("GET"));
        let timer = RegisteredFunction {
            trigger: Some("timerTrigger".into()),
            ..function
        };
        assert!(!timer.accepts("POST"));
    }

    #[test]
    fn header_values_are_ascii_json() {
        let secrets = secrets_header([("API_KEY", "s3cr€t \u{1F600}"), ("OTHER", "\"q\"")]);
        assert_eq!(
            secrets,
            r#"{"API_KEY":"s3cr\u20act \ud83d\ude00","OTHER":"\"q\""}"#
        );
        let parsed: Value = serde_json::from_str(&secrets).unwrap();
        assert_eq!(parsed["API_KEY"], "s3cr€t \u{1F600}");

        assert_eq!(
            rayfin_info_header("http://127.0.0.1:7000", "pk", None),
            r#"{"rayFinEndpoint":"http://127.0.0.1:7000","publishableKey":"pk","rayfinToken":""}"#
        );
    }

    #[test]
    fn invocation_debug_hides_header_values_and_bindings() {
        let mut invocation =
            HostInvocation::new("tokens", "{}").header("authorization", "Bearer secret-jwt");
        invocation
            .bindings
            .insert("FabricItemInput1".into(), Value::from("secret-token"));
        let printed = format!("{invocation:?}");
        assert!(!printed.contains("secret-jwt"));
        assert!(!printed.contains("secret-token"));
        assert!(printed.contains("authorization"));
    }
}
