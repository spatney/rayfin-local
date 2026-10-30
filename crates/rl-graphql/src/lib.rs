//! The DAB-compatible GraphQL data API (`POST /graphql`), compiled to SQL over the project's
//! SQLite database, with `@role` policies and field permissions enforced in every statement.
//!
//! On Fabric an app's entities are served by Data API Builder (DAB, on Hot Chocolate) over
//! Fabric SQL. [`DataApi`] answers the documents the Rayfin SDK 1.36.2 sends with the same
//! schema, data, errors and statuses; "compat" is micro-fabric's `docs/compatibility.md`.
//!
//! # Pipeline
//!
//! | Module | Role |
//! | --- | --- |
//! | [`schema`] | The schema DAB generates for the model (types, fields, arguments, filter and `orderBy` inputs), built once per model snapshot. Each object field carries a [`schema::Resolver`]. |
//! | `value` | Argument literals re-read from the document with positions (duplicate input fields kept, for DAB's error). |
//! | `validate`, `coerce` | Hot Chocolate's validation rules and messages; input coercion into [`Input`]. |
//! | [`access`] | The grant a role acts under for an action (DAB role inheritance, compat §7.2). |
//! | [`policy`] | `@role` policies compiled to SQL predicates, claims bound and typed per field. |
//! | [`plan`] | A root field → selections, filters, order and page, authorization decided per field. |
//! | [`sql`] | Parameterized SQL text and predicates ([`sql::Pred`]). |
//! | `cursor` | DAB's pagination cursors (base64 JSON of the order-by values and directions). |
//! | `exec` | One statement per root field, run in one database call, and the response shaping (errors, `null` propagation, `hasNextPage`, `endCursor`). |
//! | `aggregate` | `{E}Connection.groupBy` (DAB's aggregations): its types, plan, SQL (`GROUP BY`/`HAVING` over the connection's rows) and response. |
//! | `mutation` | `create{E}`/`update{E}`/`delete{E}`: each root field in its own transaction, per-action grants and policies, then the row read back with the read permission. |
//! | `http` | The routes and the HTTP status policy. |
//!
//! # Notes
//!
//! - **Mutations** reuse [`access::grant`], [`policy::action_filter`] for update/delete
//!   `WHERE`s, [`policy::compile`] over the candidate row for create checks, and
//!   [`plan::object_plan`] to read the row back.
//! - **Aggregations** add `groupBy` on `{E}Connection` and the `{E}GroupBy`, `{E}Aggregations`,
//!   `{E}ScalarFields`, `{E}NumericAggregateFields` types in [`schema::Schema::build`]; a
//!   [`plan::ConnectionField::GroupBy`] reuses the connection's `predicate` (filter and read
//!   policy).
//! - **Introspection**: `__schema`/`__type` are refused with Hot Chocolate's `HC0046` error
//!   unless [`DataApiSettings::allow_introspection`]; when allowed they answer an error until a
//!   resolver over [`schema::Schema`] is added (`exec::plan_root`). `__typename` always works.

pub mod access;
mod aggregate;
mod coerce;
mod cursor;
pub mod error;
mod exec;
mod http;
mod mutation;
pub mod plan;
pub mod policy;
pub mod schema;
pub mod sql;
pub mod tsql;
mod validate;
mod value;

use std::sync::Arc;

use axum::Router;
use axum::response::Response;
use axum::routing::get;
use parking_lot::RwLock;
use rl_db::Db;
use rl_model::{AppModel, Caller, CallerResolver};

pub use coerce::Input;
pub use exec::{Outcome, Request};

/// Reads a `POST` body as Hot Chocolate's request parser does; `Err` is its `400` answer. For
/// other GraphQL routes over the same front end (connectors).
pub fn read_request(body: &[u8]) -> Result<Request, Box<Response>> {
    http::read_request(body)
}

/// The HTTP response of an [`Outcome`], with the data API's status policy.
pub fn outcome_response(outcome: Outcome) -> Response {
    http::respond(outcome)
}

/// The `401` for a bearer token that is malformed, expired or revoked.
pub fn invalid_token_response() -> Response {
    http::invalid_token()
}

/// The data API's options (`services.data` in `rayfin.yml`, DAB's runtime options).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataApiSettings {
    /// `services.data.allowIntrospection` (Rayfin's default: `false`).
    pub allow_introspection: bool,
    /// DAB's maximum page size (`first: -1`), 100,000 by default.
    pub max_page_size: u32,
    /// DAB's page size without `first`, also for nested lists: 100 by default.
    pub default_page_size: u32,
}

impl Default for DataApiSettings {
    fn default() -> Self {
        Self {
            allow_introspection: false,
            max_page_size: 100_000,
            default_page_size: 100,
        }
    }
}

/// A model and the schema built from it.
pub(crate) struct Snapshot {
    pub model: Arc<AppModel>,
    pub schema: schema::Schema,
}

impl Snapshot {
    fn new(model: Arc<AppModel>) -> Self {
        Self::with_options(model, schema::SchemaOptions::default())
    }

    pub(crate) fn with_options(model: Arc<AppModel>, options: schema::SchemaOptions) -> Self {
        let schema = schema::Schema::build_with(&model, options);
        Self { model, schema }
    }
}

/// The data API: `POST /graphql` over the project database.
pub struct DataApi {
    db: Db,
    snapshot: RwLock<Arc<Snapshot>>,
    callers: Arc<dyn CallerResolver>,
    settings: DataApiSettings,
}

impl DataApi {
    pub fn new(
        db: Db,
        model: Arc<AppModel>,
        callers: Arc<dyn CallerResolver>,
        settings: DataApiSettings,
    ) -> Self {
        Self {
            db,
            snapshot: RwLock::new(Arc::new(Snapshot::new(model))),
            callers,
            settings,
        }
    }

    /// Serves `model` from now on (after a re-analysis); requests in flight finish with the
    /// model they started with.
    pub fn set_model(&self, model: Arc<AppModel>) {
        let snapshot = Arc::new(Snapshot::new(model));
        *self.snapshot.write() = snapshot;
    }

    /// The model being served.
    pub fn model(&self) -> Arc<AppModel> {
        Arc::clone(&self.snapshot.read().model)
    }

    /// Runs a GraphQL request for `caller` (the routes, and in-process callers such as tests).
    pub async fn execute(&self, caller: &Caller, request: Request) -> Outcome {
        let snapshot = Arc::clone(&self.snapshot.read());
        exec::execute(snapshot, &self.db, caller, &self.settings, request).await
    }

    /// `POST /graphql` and `GET /graphql`, state applied.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/graphql", get(http::get).post(http::post))
            .with_state(self)
    }
}
