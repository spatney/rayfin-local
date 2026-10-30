//! Category A connectors: `POST /connectors/<name>/graphql` over the connector's real Fabric SQL
//! source (a SQL database, a Warehouse or a Lakehouse's SQL analytics endpoint), as the
//! developer.
//!
//! Deployed, the Fabric GraphQL workload serves the DAB config `rayfin up` builds from the
//! connector's entity classes. Locally, `rl-graphql`'s front end and T-SQL emitter
//! ([`rl_graphql::tsql`]) answer the same documents from the same entities ([`SqlConnectorModel`]),
//! with `@role` grants and policies, and the statements run here over TDS ([`tds`]) against the
//! database the Fabric REST API names for the connector's item ([`location`]).
//!
//! Identity differs: the database sees the developer (an Entra token from the
//! [`TokenSource`]), for `delegated` and `application` connectors alike. The roles and policies
//! still act on the app's signed-in user.

pub mod location;
pub mod tds;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::response::Response;
use rl_graphql::tsql::{Target, TsqlApi};
use rl_graphql::{DataApiSettings, Outcome};
use rl_model::{Action, ActionPermission, AppModel, Caller};
use rl_tokens::{Audience, TokenSource};
use tokio::sync::Mutex;

use crate::ConnectorDef;
use location::{Item, Locator};
use tds::{Auth, Context, Endpoint, Pool, TdsBackend};

/// The Fabric REST origin locations are resolved on.
pub const DEFAULT_FABRIC_API_BASE_URL: &str = "https://api.fabric.microsoft.com";

/// A Category A connector type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SqlKind {
    /// `fabric-sqldatabase`.
    SqlDatabase,
    /// `fabric-warehouse`.
    Warehouse,
    /// `fabric-sqlanalytics`: read-only.
    SqlAnalytics,
}

impl SqlKind {
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "fabric-sqldatabase" => Some(Self::SqlDatabase),
            "fabric-warehouse" => Some(Self::Warehouse),
            "fabric-sqlanalytics" => Some(Self::SqlAnalytics),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SqlDatabase => "fabric-sqldatabase",
            Self::Warehouse => "fabric-warehouse",
            Self::SqlAnalytics => "fabric-sqlanalytics",
        }
    }

    /// The `@microsoft/rayfin-core` analyzer dialect of the type (`CONNECTOR_CATALOG`).
    pub fn analyzer_dialect(self) -> &'static str {
        match self {
            Self::SqlDatabase => "mssql",
            Self::Warehouse => "fabric-warehouse",
            Self::SqlAnalytics => "fabric-sqlanalytics",
        }
    }

    /// The Fabric item, for messages.
    pub fn item_name(self) -> &'static str {
        match self {
            Self::SqlDatabase => "SQL database",
            Self::Warehouse => "warehouse",
            Self::SqlAnalytics => "SQL analytics endpoint",
        }
    }

    /// The operations the type allows (`CONNECTOR_CATALOG[type].allowedOperations`).
    pub fn allowed_actions(self) -> &'static [Action] {
        match self {
            Self::SqlAnalytics => &[Action::Read],
            _ => &[Action::Read, Action::Create, Action::Update, Action::Delete],
        }
    }

    fn target(self) -> Target {
        match self {
            Self::SqlDatabase => Target::SqlDatabase,
            Self::Warehouse => Target::Warehouse,
            Self::SqlAnalytics => Target::SqlAnalyticsEndpoint,
        }
    }
}

/// What a Category A connector serves: its entities' model, or why there is none (its analysis
/// failed).
pub type SqlConnectorModel = Result<Arc<AppModel>, String>;

/// Where a connector's database is, set instead of resolving it in Fabric (a local SQL Server,
/// tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlServerOverride {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub auth: Auth,
    pub trust_server_certificate: bool,
}

/// The Category A part of [`crate::ConnectorsConfig`].
#[derive(Clone, Debug, PartialEq)]
pub struct SqlConnectorsConfig {
    /// The analyzed entities of each Category A connector, by connector name.
    pub models: HashMap<String, SqlConnectorModel>,
    /// The Fabric REST origin (`/v1/...` is appended).
    pub fabric_api_base_url: String,
    /// The scope of the developer token for the Fabric REST API.
    pub fabric_scope: String,
    /// The scope of the developer token the database receives.
    pub sql_scope: String,
    /// `.rayfin-local/cache`, where resolved locations are kept (`None`: not kept).
    pub cache_dir: Option<PathBuf>,
    /// Databases to use instead of the connectors' Fabric items, by connector name.
    pub servers: HashMap<String, SqlServerOverride>,
    /// Page sizes (DAB's defaults).
    pub settings: DataApiSettings,
}

impl Default for SqlConnectorsConfig {
    fn default() -> Self {
        Self {
            models: HashMap::new(),
            fabric_api_base_url: DEFAULT_FABRIC_API_BASE_URL.to_owned(),
            fabric_scope: Audience::Fabric.default_scope().to_owned(),
            sql_scope: Audience::Sql.default_scope().to_owned(),
            cache_dir: None,
            servers: HashMap::new(),
            settings: DataApiSettings::default(),
        }
    }
}

/// Restricts the entities' grants to the actions the connector declares in `rayfin.yml`
/// (`operations:`, all of the type's when none) and its type allows: `*` becomes those actions,
/// others are dropped. Returns what was dropped, as warnings (`rayfin up` refuses such a config).
pub fn restrict_to_operations(
    model: &mut AppModel,
    connector: &ConnectorDef,
    kind: SqlKind,
) -> Vec<String> {
    let declared: Vec<Action> = if connector.operations.is_empty() {
        kind.allowed_actions().to_vec()
    } else {
        connector
            .operations
            .iter()
            .filter_map(|op| Action::parse(&op.trim().to_ascii_lowercase()))
            .filter(|action| kind.allowed_actions().contains(action))
            .collect()
    };
    let mut warnings = Vec::new();
    for entity in &mut model.entities {
        for permission in &mut entity.permissions {
            let mut kept: Vec<ActionPermission> = Vec::new();
            for grant in permission.actions.drain(..) {
                if grant.action == Action::All {
                    kept.extend(declared.iter().map(|action| ActionPermission {
                        action: *action,
                        ..grant.clone()
                    }));
                } else if declared.contains(&grant.action) {
                    kept.push(grant);
                } else {
                    warnings.push(format!(
                        "Connector '{}': entity {} grants `{}` to {}, which isn't among the \
                         connector's operations ({}); rayfin-local ignores it, and `rayfin up` \
                         refuses it. Add the operation to the connector in rayfin/rayfin.yml, or \
                         narrow the @role.",
                        connector.name,
                        entity.name,
                        grant.action.as_str(),
                        permission.role,
                        declared
                            .iter()
                            .map(|action| action.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            permission.actions = kept;
        }
    }
    warnings
}

/// The Category A connectors of a router.
pub(crate) struct SqlConnectors {
    connectors: HashMap<String, Arc<SqlConnector>>,
}

struct SqlConnector {
    def: ConnectorDef,
    kind: SqlKind,
    api: Result<TsqlApi, String>,
    database: Database,
}

/// How a connector reaches its database: an endpoint, resolved on first use.
struct Database {
    tokens: Arc<dyn TokenSource>,
    locator: Result<Arc<Locator>, String>,
    sql_scope: String,
    server: Option<SqlServerOverride>,
    pool: Mutex<Option<Arc<Pool>>>,
}

impl SqlConnectors {
    pub(crate) fn new(
        config: SqlConnectorsConfig,
        connectors: &[ConnectorDef],
        tokens: Arc<dyn TokenSource>,
    ) -> Self {
        let locator = Locator::new(
            &config.fabric_api_base_url,
            config.fabric_scope.clone(),
            config.cache_dir.clone(),
        )
        .map(Arc::new);
        let mut map = HashMap::new();
        for def in connectors {
            let Some(kind) = SqlKind::parse(&def.kind) else {
                continue;
            };
            let api = match config.models.get(&def.name) {
                Some(Ok(model)) => Ok(TsqlApi::new(
                    Arc::clone(model),
                    kind.target(),
                    config.settings.clone(),
                )),
                Some(Err(problem)) => Err(problem.clone()),
                None => Err(format!(
                    "The entities of connector '{}' weren't analyzed; restart rayfin-local.",
                    def.name
                )),
            };
            let database = Database {
                tokens: Arc::clone(&tokens),
                locator: locator.clone(),
                sql_scope: config.sql_scope.clone(),
                server: config.servers.get(&def.name).cloned(),
                pool: Mutex::new(None),
            };
            map.insert(
                def.name.clone(),
                Arc::new(SqlConnector {
                    def: def.clone(),
                    kind,
                    api,
                    database,
                }),
            );
        }
        Self { connectors: map }
    }

    /// Answers `POST /connectors/<name>/graphql` for a Category A connector; `None` when `name`
    /// isn't one.
    pub(crate) async fn graphql(
        &self,
        name: &str,
        caller: &Caller,
        body: &Bytes,
    ) -> Option<Response> {
        let connector = self.connectors.get(name)?;
        Some(connector.graphql(caller, body).await)
    }
}

impl SqlConnector {
    async fn graphql(&self, caller: &Caller, body: &Bytes) -> Response {
        let api = match &self.api {
            Ok(api) => api,
            Err(problem) => {
                return crate::invoke::service_error(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "CONNECTOR_MISCONFIGURED",
                    format!("Connector '{}' can't run locally: {problem}", self.def.name),
                );
            }
        };
        if crate::invoke::runs_as_signed_in_user(&self.def, false) && !caller.is_authenticated() {
            return crate::invoke::session_required(&self.def);
        }
        let request = match rl_graphql::read_request(body) {
            Ok(request) => request,
            Err(response) => return *response,
        };
        let outcome = match self.database.pool(&self.def, self.kind).await {
            Ok(pool) => api.execute(caller, request, &TdsBackend(pool)).await,
            Err(problem) => {
                tracing::warn!("{problem}");
                unavailable(problem)
            }
        };
        rl_graphql::outcome_response(outcome)
    }
}

impl Database {
    /// The pool of the connector's database, resolving where it is on first use.
    async fn pool(&self, def: &ConnectorDef, kind: SqlKind) -> Result<Arc<Pool>, String> {
        let mut pool = self.pool.lock().await;
        if let Some(pool) = pool.as_ref() {
            return Ok(Arc::clone(pool));
        }
        let config = |key: &str| {
            def.config
                .get(key)
                .and_then(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        let (workspace, item) = (config("workspaceId"), config("itemId"));
        let describe = format!(
            "{} {} in workspace {}",
            kind.item_name(),
            item.unwrap_or("?"),
            workspace.unwrap_or("?")
        );
        let endpoint = match &self.server {
            Some(server) => Endpoint {
                host: server.host.clone(),
                port: server.port,
                database: server.database.clone(),
                auth: server.auth.clone(),
                trust_server_certificate: server.trust_server_certificate,
            },
            None => {
                let (Some(workspace), Some(item)) = (workspace, item) else {
                    return Err(format!(
                        "Connector '{}' needs config.workspaceId and config.itemId in \
                         rayfin/rayfin.yml (`rayfin connector add` writes them).",
                        def.name
                    ));
                };
                let locator = self.locator.as_ref().map_err(Clone::clone)?;
                let item = Item {
                    connector: &def.name,
                    kind,
                    workspace,
                    id: item,
                };
                let location = locator.locate(self.tokens.as_ref(), &item).await?;
                tracing::info!(
                    connector = %def.name,
                    "connector {} uses {}:{} / {}",
                    def.name,
                    location.host,
                    location.port,
                    location.database
                );
                Endpoint {
                    host: location.host,
                    port: location.port,
                    database: location.database,
                    auth: Auth::Entra {
                        scope: self.sql_scope.clone(),
                    },
                    trust_server_certificate: false,
                }
            }
        };
        let context = Context {
            connector: def.name.clone(),
            item: describe,
        };
        let created = Pool::new(endpoint, context, Arc::clone(&self.tokens));
        *pool = Some(Arc::clone(&created));
        Ok(created)
    }
}

/// A GraphQL answer for a database that can't be used.
fn unavailable(message: String) -> Outcome {
    let error = rl_graphql::error::GraphQlError::dab(
        message,
        rl_graphql::error::codes::DATABASE_OPERATION_FAILED,
    );
    Outcome::Graphql {
        status: 200,
        body: serde_json::json!({ "errors": [error.to_json()], "data": null }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory() -> AppModel {
        let json = include_str!("../../../rl-model/tests/fixtures/inventory.connector.json");
        AppModel::from_connector_analysis(&serde_json::from_str(json).unwrap()).unwrap()
    }

    fn connector(operations: &[&str]) -> ConnectorDef {
        ConnectorDef {
            name: "inventory".into(),
            kind: "fabric-sqldatabase".into(),
            operations: operations.iter().map(|op| op.to_string()).collect(),
            ..ConnectorDef::default()
        }
    }

    #[test]
    fn grants_are_limited_to_the_declared_operations() {
        let mut model = inventory();
        let warnings =
            restrict_to_operations(&mut model, &connector(&["read"]), SqlKind::SqlDatabase);
        let product = model.entity("Product").unwrap();
        assert!(
            product
                .grants("authenticated", Action::Read)
                .next()
                .is_some()
        );
        assert!(
            product
                .grants("authenticated", Action::Update)
                .next()
                .is_none()
        );
        assert_eq!(warnings.len(), 6, "{warnings:?}");
        assert!(
            warnings[0].contains("entity Product grants `update`"),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn the_analytics_endpoint_is_read_only() {
        let mut model = inventory();
        restrict_to_operations(&mut model, &connector(&[]), SqlKind::SqlAnalytics);
        let stock = model.entity("StockLevel").unwrap();
        assert!(stock.grants("authenticated", Action::Read).next().is_some());
        assert!(
            stock
                .grants("authenticated", Action::Create)
                .next()
                .is_none()
        );
    }
}
