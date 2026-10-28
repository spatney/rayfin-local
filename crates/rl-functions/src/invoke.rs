//! The function invoke routes, `POST /functions/<name>/invoke` and
//! `POST /.rayfin-local/functions/api/<name>`, and [`FunctionsService`], which keeps the app's
//! functions built, described (`runtimemetadata.json`) and running on a [`HostManager`].
//!
//! # The wire contract (1.36.2)
//!
//! The SDK posts `params ?? {}` as JSON with a 250 s timeout (SDK: `FunctionClient.js`
//! `invoke`). On a 2xx it reads the UDF envelope: a non-empty `errors` throws
//! `FunctionsError("Function invocation failed: <errors[0]>")`, a `status` other than
//! `Succeeded` throws too, else it returns `output` (a JSON string `{"output": v}` is unwrapped).
//! Any other status throws `NetworkError(message, status)`, the message read from
//! `error_description`, `message` or the envelope's `errors[]`, and a 401 first refreshes the
//! session and replays the call once (SDK: `rayfin-lib` `ApiClient.js` `handleResponseErrors`,
//! `fetchWithTimeout`). So the deployed behavior is mirrored as follows:
//!
//! - **Envelopes are always HTTP 200.** The host runs with `IsLocal=false`, even when
//!   `local.settings.json` (as `rayfin dev` writes it) says `true`: with `IsLocal === 'true'` the
//!   UDF package answers 400/422/500/403 for failed envelopes (UDF:
//!   `internal/ensureFormattedReturnType.js`), which the SDK turns into an opaque
//!   `NetworkError` instead of the `FunctionsError` a deployed app gets. The function's status,
//!   `content-type` (`application/json` when it set none) and `x-fabric-udf-status` pass through
//!   with the body; the deployed workflow adds nothing the SDK reads (Rayfin:
//!   `Workflow/FunctionInvokeWorkflow.cs` `CopyHttpResponseAsync`).
//! - **Caller.** Anonymous calls are allowed: the deployed authentication middleware gives
//!   token-less requests anonymous claims (Rayfin:
//!   `Auth/Middleware/RayfinAuthenticationMiddleware.cs`) and `InvokeController` has no
//!   `[Authorize]`. An invalid bearer token is `401 {"error": "Invalid authentication token"}`,
//!   as that middleware answers.
//! - **Request checks,** in the deployed order: the bearer token (401); the name,
//!   `^[A-Za-z0-9_]{1,128}$` (400 `FUNCTION_INVALID_NAME`, Rayfin:
//!   `Validation/FunctionInputValidation.cs`); `Content-Type: application/json` (415
//!   `FUNCTION_INVOKE_INVALID_CONTENT_TYPE`). Then rayfin-local's own: a body of at most 30 MiB
//!   (413), valid JSON (400; an empty body means `{}`).
//! - **Errors without an envelope** use the deployed shape `{"error": <code>, "message"}`
//!   (Rayfin: `Middleware/RayfinExceptionMiddleware.cs`): unknown function 404
//!   `FUNCTION_NOT_FOUND`; host (re)starting 503 `FUNCTION_HOST_STARTING` with `Retry-After`;
//!   functions not built or not loadable 503 `FUNCTIONS_UNAVAILABLE`; past the timeout (250 s,
//!   the UDF ceiling) 408 `FUNCTION_INVOKE_TIMEOUT`; no response from the function (a crash, a
//!   `BigInt` result) 500 `FUNCTION_INVOKE_FAILED`. Deployed 5xx bodies hide their cause behind
//!   `internal_server_error`; locally the message says what failed.
//!
//! # What the function gets
//!
//! A `POST` with the body, the caller's `Content-Type` and `Authorization` (the context's token
//! in Development mode), `x-ms-rayfin-info` (`{rayFinEndpoint, publishableKey, rayfinToken}`,
//! the context in deployed mode) and `x-ms-rayfin-secrets` (the declared secrets, ASCII-escaped).
//! For each audience the function declares, a token from the [`TokenSource`] (the audience's
//! default scope, which the broker maps to an override) arrives as its `FabricItem` binding;
//! when one can't be minted the function still runs and reading `ctx.Tokens.<Audience>` throws
//! "No token available…", and rayfin-local warns with the token source's hint.
//!
//! The host environment is `local.settings.json`'s `Values`, then the declared secrets, then:
//! `RAYFIN_API_URL` (this backend, with a trailing `/`), `RAYFIN_PUBLISHABLE_KEY`,
//! `RAYFIN_FABRIC_WORKSPACE_ID`, `RAYFIN_FABRIC_ITEM_ID`, `IsLocal=false`, an empty
//! `RAYFIN_ACCESS_TOKEN` (in Development mode the UDF package would otherwise give anonymous
//! callers the token `rayfin dev` stored there), and `AZURE_FUNCTIONS_ENVIRONMENT=Development`
//! unless `Values` sets it.
//!
//! Sources: SDK is the 1.36.2 dist of `@microsoft/rayfin-functions` and `@microsoft/rayfin-lib`,
//! UDF that of `@microsoft/fabric-user-data-functions`, Rayfin its `packages/host` source.
//!
//! [`HostManager`]: crate::host::HostManager
//! [`TokenSource`]: rl_tokens::TokenSource

mod routes;
mod service;
mod tokens;

use std::{path::PathBuf, time::Duration};

use rl_node::{LineSink, NodeRuntime};

pub use service::FunctionsService;

/// The UDF invocation ceiling, which the SDK's own timeout matches.
pub const DEFAULT_INVOKE_TIMEOUT: Duration = Duration::from_secs(250);

/// The largest request body accepted: the UDF package's response cap.
pub const MAX_BODY_BYTES: usize = 30 * 1024 * 1024;

/// What [`FunctionsService::start`] needs.
#[derive(Clone)]
pub struct FunctionsConfig {
    pub node: NodeRuntime,
    /// The functions package (`services.functions.path`).
    pub functions_dir: PathBuf,
    /// `services.functions.buildCommand` (default `npm run build`).
    pub build_command: Option<String>,
    /// `<project>/.rayfin-local/cache`: helper scripts and the metadata stamp.
    pub cache_dir: PathBuf,
    /// This backend's origin, such as `http://localhost:7171`.
    pub api_origin: String,
    pub publishable_key: String,
    pub workspace_id: String,
    pub item_id: String,
    /// The secrets `rayfin.yml` declares, with their values.
    pub secrets: Vec<(String, String)>,
    /// Starts the Node.js inspector on `127.0.0.1:<port>`.
    pub inspect_port: Option<u16>,
    /// How long an invocation may run (408 after).
    pub invoke_timeout: Duration,
    /// Receives the output of the build (`build`), the metadata helper and the host (`fn`).
    pub sink: LineSink,
}

impl FunctionsConfig {
    /// A config with the default build, no secrets, no inspector and the 250 s timeout.
    pub fn new(
        node: NodeRuntime,
        functions_dir: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
        api_origin: impl Into<String>,
        publishable_key: impl Into<String>,
    ) -> Self {
        Self {
            node,
            functions_dir: functions_dir.into(),
            build_command: None,
            cache_dir: cache_dir.into(),
            api_origin: api_origin.into(),
            publishable_key: publishable_key.into(),
            workspace_id: String::new(),
            item_id: String::new(),
            secrets: Vec::new(),
            inspect_port: None,
            invoke_timeout: DEFAULT_INVOKE_TIMEOUT,
            sink: rl_node::default_sink(),
        }
    }
}

impl std::fmt::Debug for FunctionsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Secret values stay out of logs: names only.
        f.debug_struct("FunctionsConfig")
            .field("functions_dir", &self.functions_dir)
            .field("build_command", &self.build_command)
            .field("api_origin", &self.api_origin)
            .field(
                "secrets",
                &self.secrets.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("inspect_port", &self.inspect_port)
            .field("invoke_timeout", &self.invoke_timeout)
            .finish_non_exhaustive()
    }
}

/// The functions can't be served at all; build and load failures are not errors (the service
/// starts and answers 503 until the package is fixed).
#[derive(Debug, thiserror::Error)]
pub enum FunctionsError {
    #[error(
        "the functions folder {} does not exist (check services.functions.path in rayfin.yml)",
        .0.display()
    )]
    MissingDir(PathBuf),
    /// `package.json` is missing or unusable, so the package can't be built or watched.
    #[error(transparent)]
    Package(#[from] crate::builder::BuildError),
    #[error("could not read the functions' local.settings.json: {0}")]
    Settings(#[source] std::io::Error),
}
