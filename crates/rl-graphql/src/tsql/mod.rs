//! DAB-compatible GraphQL over a Fabric SQL source, in T-SQL: the API of Category A connectors
//! (`fabric-sqldatabase`, `fabric-warehouse`, `fabric-sqlanalytics`).
//!
//! Requests go through the data API's front end ([`crate::exec::prepare`], the planner, its
//! policies and errors) against the schema DAB generates for the connector's entities, then:
//!
//! - **Queries**: one statement per root field ([`emit`]), its rows' JSON normalized to DAB's
//!   formats, then shaped by the data API's shaper.
//! - **Mutations** ([`mutation`]): each root field in its own transaction. On a SQL database the
//!   row is read back by its key, as DAB's MSSQL engine does; a Warehouse answers
//!   `DbOperationResult { result: "success" }`; an analytics endpoint has none.
//!
//! The statements run on a [`backend::SqlBackend`], which `rl-connectors` implements over TDS.

pub mod backend;
mod emit;
mod mutation;

use std::sync::Arc;

use async_graphql_parser::types::OperationType;
use rl_model::{AppModel, Caller};
use serde_json::Value as Json;

use crate::error::{GraphQlError, codes};
use crate::exec::{self, Outcome, QueryRoot, Request, RootKind};
use crate::schema::{Mutations, SchemaOptions};
use crate::value::Source;
use crate::{DataApiSettings, Snapshot};
use backend::{SqlBackend, SqlError, Statement, Value};

/// The kind of Fabric SQL source, which decides what mutations do (DAB's database type).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A Fabric SQL database (DAB `mssql`): mutations return the row.
    SqlDatabase,
    /// A Fabric Warehouse (DAB `dwsql`): mutations return `DbOperationResult`.
    Warehouse,
    /// A Lakehouse's SQL analytics endpoint: read-only.
    SqlAnalyticsEndpoint,
}

impl Target {
    fn schema_options(self) -> SchemaOptions {
        SchemaOptions {
            mutations: match self {
                Self::SqlDatabase => Mutations::Row,
                Self::Warehouse => Mutations::OperationResult,
                Self::SqlAnalyticsEndpoint => Mutations::None,
            },
        }
    }
}

/// The GraphQL API of one connector: its entities' schema, over a [`SqlBackend`].
pub struct TsqlApi {
    snapshot: Arc<Snapshot>,
    settings: DataApiSettings,
    target: Target,
}

impl TsqlApi {
    pub fn new(model: Arc<AppModel>, target: Target, settings: DataApiSettings) -> Self {
        Self {
            snapshot: Arc::new(Snapshot::with_options(model, target.schema_options())),
            settings,
            target,
        }
    }

    pub fn model(&self) -> &AppModel {
        &self.snapshot.model
    }

    pub fn target(&self) -> Target {
        self.target
    }

    /// Runs a GraphQL request for `caller` on `backend`.
    pub async fn execute(
        &self,
        caller: &Caller,
        request: Request,
        backend: &dyn SqlBackend,
    ) -> Outcome {
        let snapshot = &self.snapshot;
        let model = &snapshot.model;
        let schema = &snapshot.schema;
        let parsed = match exec::prepare(schema, &self.settings, &request) {
            Ok(parsed) => parsed,
            Err(outcome) => return outcome,
        };
        let source = Source::new(&request.query);
        let op = parsed.operation();
        if op.ty == OperationType::Mutation {
            let planned = {
                let ctx = parsed.ctx(model, schema, caller, &self.settings, &source);
                mutation::plan(&ctx, &op.selection_set.node, self.target)
            };
            return mutation::execute(model, planned, backend).await;
        }
        let roots = {
            let ctx = parsed.ctx(model, schema, caller, &self.settings, &source);
            match exec::plan_query(&ctx, op) {
                Ok(roots) => roots,
                Err(outcome) => return outcome,
            }
        };
        let mut rows = Vec::with_capacity(roots.len());
        for root in &roots {
            rows.push(match root_statement(model, root) {
                None => None,
                Some(Err(error)) => Some(Err(error)),
                Some(Ok(statement)) => Some(run_query(model, root, &statement, backend).await),
            });
        }
        exec::shape_query(model, roots, rows)
    }
}

/// The statement of a root field (`None` when it reads nothing).
fn root_statement(model: &AppModel, root: &QueryRoot) -> Option<Result<Statement, GraphQlError>> {
    let mut emitter = emit::Emitter::new(model);
    let text = match &root.kind {
        RootKind::List(plan) if emit::uses_group_by(plan) => return Some(Err(not_supported())),
        RootKind::List(plan) => emitter.connection_select(plan, false),
        RootKind::ByPk(plan, _) if emit::object_uses_group_by(plan) => {
            return Some(Err(not_supported()));
        }
        RootKind::ByPk(plan, filter) => emitter.object_select(plan, filter),
        RootKind::Typename | RootKind::Failed(_) => return None,
    };
    Some(Ok(emitter.finish(text)))
}

fn not_supported() -> GraphQlError {
    GraphQlError::dab(
        "Aggregations (groupBy) on connector entities don't run in rayfin-local yet; deploy \
         with `npx rayfin up` to use them.",
        codes::BAD_REQUEST,
    )
}

async fn run_query(
    model: &AppModel,
    root: &QueryRoot,
    statement: &Statement,
    backend: &dyn SqlBackend,
) -> Result<Vec<Json>, GraphQlError> {
    let rows = backend
        .query(statement)
        .await
        .map_err(|error| sql_error(&root.key, error))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut json = json_column(&row)?;
        match &root.kind {
            RootKind::List(plan) => emit::normalize_connection(model, plan, &mut json),
            RootKind::ByPk(plan, _) => emit::normalize_object(model, plan, &mut json),
            _ => {}
        }
        out.push(json);
    }
    Ok(out)
}

/// The JSON a statement built in its first column.
fn json_column(row: &[Value]) -> Result<Json, GraphQlError> {
    match row.first() {
        Some(Value::Text(text)) => serde_json::from_str(text).map_err(|error| {
            tracing::warn!("a connector statement built invalid JSON: {error}");
            exec::database_error()
        }),
        _ => Ok(Json::Null),
    }
}

/// The GraphQL error for a failed statement: DAB's for errors of the database, with its message
/// as `extensions.detail`; what to do for a database that can't be used.
pub(crate) fn sql_error(field: &str, error: SqlError) -> GraphQlError {
    match error {
        SqlError::Database { number, message } => {
            tracing::warn!(
                field,
                "a connector statement failed: {message} (error {number})"
            );
            exec::database_error().ext("detail", message)
        }
        SqlError::Unavailable(message) => {
            tracing::warn!("{message}");
            GraphQlError::dab(message, codes::DATABASE_OPERATION_FAILED)
        }
    }
}
