//! A real SQL Server for the Category A connector tests: `RAYFIN_LOCAL_TEST_MSSQL`
//! (`host,port;user;password`), a fresh database with the `inventory` connector's tables per test,
//! its model, and callers with an `email` claim.

use std::sync::Arc;

use axum::Router;
use rl_connectors::sql::tds::Auth;
use rl_connectors::sql::{SqlServerOverride, restrict_to_operations};
use rl_connectors::{ConnectorDef, ConnectorsConfig, SqlConnectorsConfig, SqlKind};
use rl_model::{AppModel, Caller, CallerError, CallerResolver, Session};
use serde_json::{Map, Value, json};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

pub const HAMMER: &str = "11111111-1111-1111-1111-111111111111";
pub const SAW: &str = "22222222-2222-2222-2222-222222222222";
pub const RAKE: &str = "33333333-3333-3333-3333-333333333333";

pub const SCHEMA: &str = "
EXEC('CREATE SCHEMA sales');
CREATE TABLE dbo.categories (
  category_id INT IDENTITY(1, 1) NOT NULL PRIMARY KEY,
  name NVARCHAR(100) NOT NULL,
  description NVARCHAR(400) NULL);
CREATE TABLE sales.products (
  product_id UNIQUEIDENTIFIER NOT NULL DEFAULT NEWID() PRIMARY KEY,
  category_id INT NOT NULL REFERENCES dbo.categories (category_id),
  name NVARCHAR(200) NOT NULL,
  price DECIMAL(10, 2) NOT NULL,
  in_stock BIT NOT NULL,
  created_at DATETIME2(7) NOT NULL DEFAULT SYSUTCDATETIME(),
  owner_email NVARCHAR(320) NOT NULL);
CREATE TABLE sales.stock_levels (
  product_id UNIQUEIDENTIFIER NOT NULL REFERENCES sales.products (product_id),
  warehouse_code NVARCHAR(10) NOT NULL,
  quantity INT NOT NULL,
  PRIMARY KEY (product_id, warehouse_code));
CREATE TABLE dbo.audit_log (
  at DATETIMEOFFSET(7) NOT NULL,
  action NVARCHAR(50) NOT NULL,
  detail NVARCHAR(400) NULL);
INSERT dbo.categories (name, description) VALUES (N'Tools', N'Hand tools'), (N'Garden', NULL);
INSERT sales.products (product_id, category_id, name, price, in_stock, created_at, owner_email)
VALUES
  ('11111111-1111-1111-1111-111111111111', 1, N'Hammer', 12.50, 1,
   '2024-01-15T10:30:00.1234567', N'alice@contoso.com'),
  ('22222222-2222-2222-2222-222222222222', 1, N'Saw \"Pro\" 50%', 30.00, 0,
   '2024-02-01T00:00:00', N'bob@contoso.com'),
  ('33333333-3333-3333-3333-333333333333', 2, N'Rake', 8.99, 1,
   '2024-03-10T08:00:00', N'alice@contoso.com');
INSERT sales.stock_levels (product_id, warehouse_code, quantity) VALUES
  ('11111111-1111-1111-1111-111111111111', N'SEA', 5),
  ('11111111-1111-1111-1111-111111111111', N'NYC', 0),
  ('33333333-3333-3333-3333-333333333333', N'SEA', 12);
INSERT dbo.audit_log (at, action, detail) VALUES
  ('2024-01-15T10:30:00+02:00', N'login', NULL),
  ('2024-01-16T00:00:00+00:00', N'logout', N'bye');
";

/// `RAYFIN_LOCAL_TEST_MSSQL`: `host,port;user;password`.
pub struct Server {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
}

pub fn server() -> Option<Server> {
    let Ok(value) = std::env::var("RAYFIN_LOCAL_TEST_MSSQL") else {
        eprintln!(
            "skipped: set RAYFIN_LOCAL_TEST_MSSQL=host,port;user;password to run against SQL Server"
        );
        return None;
    };
    let mut parts = value.splitn(3, ';');
    let address = parts.next()?;
    let (host, port) = address.split_once(',').unwrap_or((address, "1433"));
    Some(Server {
        host: host.to_owned(),
        port: port
            .parse()
            .expect("RAYFIN_LOCAL_TEST_MSSQL: a numeric port"),
        user: parts.next()?.to_owned(),
        password: parts.next()?.to_owned(),
    })
}

/// Runs `sql` as the server's administrator.
pub async fn admin(server: &Server, database: &str, sql: &str) {
    let mut config = Config::new();
    config.host(&server.host);
    config.port(server.port);
    config.database(database);
    config.encryption(EncryptionLevel::Required);
    config.trust_cert();
    config.authentication(AuthMethod::sql_server(&server.user, &server.password));
    let tcp = TcpStream::connect(config.get_addr()).await.unwrap();
    let mut client = Client::connect(config, tcp.compat_write()).await.unwrap();
    client
        .simple_query(sql)
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
}

/// A fresh database with the inventory tables and rows; dropped when the guard is.
pub struct Database<'a> {
    pub server: &'a Server,
    pub name: String,
}

impl<'a> Database<'a> {
    pub async fn create(server: &'a Server, test: &str) -> Self {
        let name = format!("rl_{test}_{}", std::process::id());
        let create = format!(
            "IF DB_ID('{name}') IS NOT NULL BEGIN ALTER DATABASE [{name}] SET SINGLE_USER WITH \
             ROLLBACK IMMEDIATE; DROP DATABASE [{name}]; END; CREATE DATABASE [{name}];"
        );
        admin(server, "master", &create).await;
        admin(server, &name, SCHEMA).await;
        Self { server, name }
    }

    pub async fn drop(self) {
        let drop = format!(
            "ALTER DATABASE [{0}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{0}];",
            self.name
        );
        admin(self.server, "master", &drop).await;
    }
}

/// `alice` and `bob` are signed-in users with an `email` claim; no token is anonymous.
pub struct Callers;

impl CallerResolver for Callers {
    fn resolve(&self, bearer: Option<&str>) -> Result<Caller, CallerError> {
        let user = |name: &str| {
            let mut claims = Map::new();
            claims.insert("sub".into(), json!(format!("{name}-id")));
            claims.insert("email".into(), json!(format!("{name}@contoso.com")));
            Caller::authenticated(Session {
                user_id: format!("{name}-id"),
                email: Some(format!("{name}@contoso.com")),
                claims,
                access_token: name.to_owned(),
            })
        };
        match bearer {
            None => Ok(Caller::anonymous()),
            Some(name @ ("alice" | "bob")) => Ok(user(name)),
            Some(_) => Err(CallerError::InvalidToken),
        }
    }
}

pub fn inventory() -> AppModel {
    let analysis: Value = serde_json::from_str(include_str!(
        "../../../rl-model/tests/fixtures/inventory.connector.json"
    ))
    .unwrap();
    let metadata: Value = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/apps/connectors/rayfin/connectors/inventory/metadata.json"
    ))
    .unwrap();
    let mut model = AppModel::from_connector_analysis(&analysis).unwrap();
    assert!(model.apply_source_metadata(&metadata).is_empty());
    model
}

/// The connector routes, with the `inventory` connector of type `kind` on `database`.
pub fn app(server: &Server, database: &str, kind: &str, port: Option<u16>) -> Router {
    router(server, database, &[("inventory", kind)], port)
}

/// The connector routes, with a connector over the inventory entities per (name, type), all on
/// `database` (`port` instead of the server's).
pub fn router(
    server: &Server,
    database: &str,
    connectors: &[(&str, &str)],
    port: Option<u16>,
) -> Router {
    let mut sql = SqlConnectorsConfig::default();
    let mut defs = Vec::new();
    for (name, kind) in connectors {
        let def = ConnectorDef {
            name: (*name).into(),
            kind: (*kind).into(),
            config: json!({ "workspaceId": "w", "itemId": "i" })
                .as_object()
                .unwrap()
                .clone(),
            auth_type: Some("application".into()),
            ..ConnectorDef::default()
        };
        let mut model = inventory();
        restrict_to_operations(&mut model, &def, SqlKind::parse(kind).unwrap());
        sql.models.insert((*name).to_owned(), Ok(Arc::new(model)));
        sql.servers.insert(
            (*name).to_owned(),
            SqlServerOverride {
                host: server.host.clone(),
                port: port.unwrap_or(server.port),
                database: database.to_owned(),
                auth: Auth::Password {
                    user: server.user.clone(),
                    password: server.password.clone(),
                },
                trust_server_certificate: true,
            },
        );
        defs.push(def);
    }
    let config = ConnectorsConfig {
        connectors: defs,
        sql,
        ..ConnectorsConfig::default()
    };
    rl_connectors::router(
        config,
        Arc::new(Callers),
        super::FakeTokens::with_token("unused"),
    )
}
