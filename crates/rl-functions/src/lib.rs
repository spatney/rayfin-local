//! Rayfin functions, run locally on the app's own `@microsoft/fabric-user-data-functions` code.

/// Building the functions package (`services.functions.buildCommand`, `build:watch`).
pub mod builder;
/// The Node function host (`js/function-host.mjs`) and its lifecycle.
pub mod host;
/// `POST /functions/<name>/invoke`: the invocation envelope, per-audience tokens and secrets.
pub mod invoke;
/// `runtimemetadata.json`: freshness checks and generation (`js/udf-metadata.mjs`).
pub mod metadata;

pub use invoke::{FunctionsConfig, FunctionsError, FunctionsService};
