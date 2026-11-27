//! Rayfin functions, run locally on the app's own `@microsoft/fabric-user-data-functions` code.

/// Building the functions package (`services.functions.buildCommand`, `build:watch`).
pub mod builder;
/// The Node function host (`js/function-host.mjs`) and its lifecycle.
pub mod host;
/// `POST /functions/<name>/invoke`: the invocation envelope, per-audience tokens and secrets.
pub mod invoke;
/// `runtimemetadata.json`: freshness checks and generation (`js/udf-metadata.mjs`).
pub mod metadata;
mod state;
/// `src/types.ts` and `src/secrets.generated.ts`, generated with the project's Rayfin CLI
/// (`js/functions-types.mjs`).
pub mod typegen;

pub use invoke::{FunctionsConfig, FunctionsError, FunctionsService};
pub use state::{FunctionsState, StateObserver};
pub use typegen::{TypegenObserver, TypegenOutcome};
