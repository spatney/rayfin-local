//! Rayfin connectors: `POST /connector-invoke/<name>` (Category B) and
//! `POST /connectors/<name>/graphql` (Category A).
//!
//! - `fabric-semanticmodel` queries run against the real model through Power BI's
//!   `executeDaxQueries`, as the developer (below).
//! - `fabric-sqldatabase`, `fabric-warehouse` and `fabric-sqlanalytics` answer DAB-style GraphQL
//!   from their entity classes over the real Fabric SQL source, as the developer ([`sql`]).
//! - Other connector types (`kusto`) answer 501.
//!
//! Deployed, Rayfin runs a semantic-model query as the signed-in user (on-behalf-of). Locally it
//! runs as the developer, with a token from [`TokenSource`], calling Power BI the way
//! `rayfin connector invoke` does (the SDK's direct path, `directExecute.js`). The answers are
//! shaped so the SDK's `fabricSemanticModel()` runtime (1.36.2) normalizes them exactly as it
//! normalizes that direct path:
//!
//! - A query Power BI answers streams its Apache Arrow body back unchanged. DAX errors and
//!   oversized values are inside that body, and the SDK reports them as `query` and `overflow`.
//! - Power BI refuses `executeDaxQueries` to developer tokens without a dataset scope, which is
//!   every Azure CLI token. Those queries run through `executeQueries` (JSON) instead and answer
//!   the JSON envelope the SDK also accepts: the same tables, without `dataType`s (see
//!   `semantic_model/json_fallback.rs` for the other differences).
//! - A Power BI failure (any non-2xx), an unreachable Power BI and a missing developer token
//!   answer 200 with the JSON `FabricSemanticModelTabularResponse` envelope. Its
//!   `output.responseError` carries the category (`api`, `network` or `unknown`), message, code,
//!   details and recovery hint, as the direct path builds them.
//! - Requests the service itself refuses (unknown connector, undeclared operation, no signed-in
//!   user, a bad token) answer with the deployed route's status, which the SDK throws as a
//!   `NetworkError`.

mod invoke;
mod semantic_model;
pub mod sql;

use std::sync::Arc;

use rl_model::CallerResolver;
use rl_tokens::{Audience, TokenSource};
use serde_json::{Map, Value};

pub use sql::{SqlConnectorModel, SqlConnectorsConfig, SqlKind};

/// The public Power BI REST origin `executeDaxQueries` is called on.
pub const DEFAULT_POWERBI_BASE_URL: &str = "https://api.powerbi.com";

/// Whether rayfin-local runs connectors of type `kind` (`rayfin.yml` `connectors[].type`); the
/// others answer 501.
pub fn runs_locally(kind: &str) -> bool {
    kind == "fabric-semanticmodel" || SqlKind::parse(kind).is_some()
}

/// A connector declared in `rayfin/rayfin.yml` (`connectors[]`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnectorDef {
    /// `name`: the key apps use (`client.connectors.<name>`) and the route's last segment.
    pub name: String,
    /// `type`, for example `fabric-semanticmodel` or `kusto`.
    pub kind: String,
    /// `version`, the adapter version (`"1"` for semantic models).
    pub version: Option<String>,
    /// `config`, for example `workspaceId` and `itemId`.
    pub config: Map<String, Value>,
    /// `auth.type`: `delegated` or `application`. `None` means the type's default (delegated for
    /// semantic models).
    pub auth_type: Option<String>,
    /// The names of `operations[]`. Empty means every operation of the type.
    pub operations: Vec<String>,
}

/// What [`router`] serves.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectorsConfig {
    pub connectors: Vec<ConnectorDef>,
    /// The Power BI REST origin (`/v1.0/myorg/...` is appended); configurable for tests and
    /// sovereign clouds.
    pub powerbi_base_url: String,
    /// The scope of the developer token sent to Power BI.
    pub powerbi_scope: String,
    /// Category A connectors: their entities, and how their databases are found.
    pub sql: SqlConnectorsConfig,
}

impl Default for ConnectorsConfig {
    fn default() -> Self {
        Self {
            connectors: Vec::new(),
            powerbi_base_url: DEFAULT_POWERBI_BASE_URL.to_owned(),
            powerbi_scope: Audience::PowerBi.default_scope().to_owned(),
            sql: SqlConnectorsConfig::default(),
        }
    }
}

/// `POST /connector-invoke/{name}` and `POST /connectors/{name}/graphql`.
///
/// `callers` identifies the signed-in user from the request's bearer token; `tokens` mints the
/// developer's tokens: Power BI for `fabric-semanticmodel` queries, Fabric and SQL for Category A
/// connectors.
pub fn router(
    config: ConnectorsConfig,
    callers: Arc<dyn CallerResolver>,
    tokens: Arc<dyn TokenSource>,
) -> axum::Router {
    invoke::router(config, callers, tokens)
}
