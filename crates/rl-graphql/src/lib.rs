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
//! | `http` | The routes and the HTTP status policy. |
//!
//! # Extension points (mutations, aggregations)
//!
//! - **Mutations** (`mutation.rs`): add a `Mutation` object type in [`schema::Schema::build`]
//!   (set `mutation_type`), with `create{E}(item:)`/`update{E}(id:, item:)`/`delete{E}(id:)`
//!   fields and the `Create{E}Input`/`Update{E}Input` input types; new [`schema::Resolver`]
//!   variants for them; a mutation branch in `exec::execute` (it currently answers
//!   "does not support `Mutation` operations"), running each root field in its own transaction.
//!   Reuse [`access::grant`] (create/update/delete), [`policy::action_filter`] for update/delete
//!   `WHERE`s, [`policy::compile`] with a row source of candidate values for create checks, and
//!   [`plan::object_plan`] plus `exec`'s object JSON to read the row back with the read
//!   permission. Extend `exec`'s `requires_authentication` to the write actions.
//! - **Aggregations** (`aggregate.rs`, done): `groupBy` on `{E}Connection` and the `{E}GroupBy`,
//!   `{E}Aggregations`, `{E}ScalarFields`, `{E}NumericAggregateFields` types, added in
//!   [`schema::Schema::build`] with [`schema::Resolver::Extension`]; a
//!   [`plan::ConnectionField::GroupBy`] in [`plan::connection_plan`], whose groups reuse the
//!   connection's `predicate` (filter and read policy); `exec` hands a connection with `groupBy`
//!   to `aggregate` for its SQL and shaping.
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
mod validate;
mod value;

use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use parking_lot::RwLock;
use rl_db::Db;
use rl_model::{AppModel, Caller, CallerResolver};

pub use coerce::Input;
pub use exec::{Outcome, Request};

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
        let schema = schema::Schema::build(&model);
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
