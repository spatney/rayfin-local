//! Where a Category A connector's database is: the TDS server and database of its Fabric item,
//! from the Fabric REST API with the developer's token, as `rayfin connector add` resolves them
//! (CLI 1.36.2 `SqlEndpointManager`), and cached in `.rayfin-local/cache/connectors/`.
//!
//! | Type | Request | Server | Database |
//! | --- | --- | --- | --- |
//! | `fabric-sqldatabase` | `GET workspaces/{ws}/sqlDatabases/{id}` | `properties.serverFqdn` | `properties.databaseName` |
//! | `fabric-warehouse` | `GET workspaces/{ws}/warehouses/{id}` | `properties.connectionString` | the item id |
//! | `fabric-sqlanalytics` | `GET workspaces/{ws}/sqlEndpoints/{id}/connectionString` | `connectionString` | the item id |
//!
//! `connector add` records a Lakehouse's SQL analytics endpoint id for `fabric-sqlanalytics`; a
//! Lakehouse id written by hand is followed to its endpoint
//! (`properties.sqlEndpointProperties`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{StatusCode, Url};
use rl_tokens::TokenSource;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::SqlKind;

const TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_PORT: u16 = 1433;

/// A TDS endpoint and database.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub host: String,
    pub port: u16,
    pub database: String,
}

impl Location {
    /// Reads `Data Source=host,port;Initial Catalog=db;…` (ADO.NET), `tcp:host,port` or `host`;
    /// `database` wins over the string's catalog.
    pub fn parse(connection: &str, database: Option<&str>) -> Option<Self> {
        let mut server = None;
        let mut catalog = None;
        if connection.contains('=') {
            for part in connection.split(';') {
                let Some((key, value)) = part.split_once('=') else {
                    continue;
                };
                match key.trim().to_ascii_lowercase().as_str() {
                    "data source" | "server" | "address" | "addr" => server = Some(value.trim()),
                    "initial catalog" | "database" => catalog = Some(value.trim()),
                    _ => {}
                }
            }
        } else {
            server = Some(connection.trim());
        }
        let server = server?.trim_start_matches("tcp:");
        let (host, port) = match server.split_once(',') {
            Some((host, port)) => (host.trim(), port.trim().parse().ok()?),
            None => (server, DEFAULT_PORT),
        };
        let database = database.filter(|d| !d.is_empty()).or(catalog)?;
        (!host.is_empty()).then(|| Self {
            host: host.to_owned(),
            port,
            database: database.to_owned(),
        })
    }
}

/// The cache file of a resolved location.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cached {
    connector_type: String,
    workspace_id: String,
    item_id: String,
    location: Location,
}

/// The item a location is resolved for.
pub struct Item<'a> {
    pub connector: &'a str,
    pub kind: SqlKind,
    pub workspace: &'a str,
    pub id: &'a str,
}

impl Item<'_> {
    fn describe(&self) -> String {
        format!(
            "{} {} in workspace {}",
            self.kind.item_name(),
            self.id,
            self.workspace
        )
    }
}

/// Resolves locations through the Fabric REST API.
pub struct Locator {
    client: reqwest::Client,
    base: Url,
    scope: String,
    cache_dir: Option<PathBuf>,
}

impl Locator {
    /// `base_url` is the Fabric REST origin (`https://api.fabric.microsoft.com`); `cache_dir` the
    /// folder for `connectors/<name>.json`.
    pub fn new(base_url: &str, scope: String, cache_dir: Option<PathBuf>) -> Result<Self, String> {
        // reqwest is built without a rustls provider; a no-op when one is installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let base = Url::parse(&format!("{}/v1/", base_url.trim_end_matches('/')))
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .ok_or_else(|| format!("The Fabric API URL '{base_url}' isn't an http(s) URL."))?;
        let client = reqwest::Client::builder()
            .user_agent(concat!("rayfin-local/", env!("CARGO_PKG_VERSION")))
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| format!("Couldn't create the HTTP client for the Fabric API: {e}."))?;
        Ok(Self {
            client,
            base,
            scope,
            cache_dir,
        })
    }

    fn cache_file(&self, connector: &str) -> Option<PathBuf> {
        self.cache_dir
            .as_ref()
            .map(|dir| dir.join("connectors").join(format!("{connector}.json")))
    }

    /// The cached location of `item`, when it was resolved for the same item.
    pub fn cached(&self, item: &Item<'_>) -> Option<Location> {
        let text = std::fs::read(self.cache_file(item.connector)?).ok()?;
        let cached: Cached = serde_json::from_slice(&text).ok()?;
        (cached.connector_type == item.kind.as_str()
            && cached.workspace_id.eq_ignore_ascii_case(item.workspace)
            && cached.item_id.eq_ignore_ascii_case(item.id))
        .then_some(cached.location)
    }

    /// Forgets the cached location of `connector` (it no longer answers).
    pub fn forget(&self, connector: &str) {
        if let Some(path) = self.cache_file(connector) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn store(&self, item: &Item<'_>, location: &Location) {
        let Some(path) = self.cache_file(item.connector) else {
            return;
        };
        let cached = Cached {
            connector_type: item.kind.as_str().to_owned(),
            workspace_id: item.workspace.to_owned(),
            item_id: item.id.to_owned(),
            location: location.clone(),
        };
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let bytes = serde_json::to_vec_pretty(&cached).map_err(std::io::Error::other)?;
                write_file(&path, &bytes)
            });
        if let Err(error) = written {
            tracing::debug!(
                "could not cache the location of connector {}: {error}",
                item.connector
            );
        }
    }

    /// The location of `item`, from the cache or the Fabric REST API.
    pub async fn locate(
        &self,
        tokens: &dyn TokenSource,
        item: &Item<'_>,
    ) -> Result<Location, String> {
        if let Some(location) = self.cached(item) {
            return Ok(location);
        }
        let connector = item.connector;
        let token = tokens.token(&self.scope).await.map_err(|error| {
            let hint = error.hint.map_or_else(
                || " Run `az login`, then try again.".to_owned(),
                |hint| format!(" {hint}"),
            );
            format!(
                "Connector '{connector}' needs your Azure sign-in to find its {} in Fabric: {}.{hint}",
                item.kind.item_name(),
                error.message.trim_end_matches('.')
            )
        })?;
        let (workspace, id) = (item.workspace, item.id);
        let location = match item.kind {
            SqlKind::SqlDatabase => {
                let path = format!("workspaces/{workspace}/sqlDatabases/{id}");
                let body = self.get(&token.token, item, &path).await?;
                let properties = &body["properties"];
                properties["serverFqdn"]
                    .as_str()
                    .or_else(|| properties["connectionString"].as_str())
                    .and_then(|server| Location::parse(server, properties["databaseName"].as_str()))
            }
            SqlKind::Warehouse => {
                let path = format!("workspaces/{workspace}/warehouses/{id}");
                let body = self.get(&token.token, item, &path).await?;
                body["properties"]["connectionString"]
                    .as_str()
                    .and_then(|server| Location::parse(server, Some(id)))
            }
            SqlKind::SqlAnalytics => {
                let path = format!("workspaces/{workspace}/sqlEndpoints/{id}/connectionString");
                match self.get(&token.token, item, &path).await {
                    Ok(body) => body["connectionString"]
                        .as_str()
                        .and_then(|server| Location::parse(server, Some(id))),
                    Err(problem) => {
                        // A Lakehouse id instead of its SQL analytics endpoint's.
                        let path = format!("workspaces/{workspace}/lakehouses/{id}");
                        let Ok(body) = self.get(&token.token, item, &path).await else {
                            return Err(problem);
                        };
                        let endpoint = &body["properties"]["sqlEndpointProperties"];
                        match (
                            endpoint["connectionString"].as_str(),
                            endpoint["id"].as_str(),
                        ) {
                            (Some(server), Some(endpoint)) => {
                                Location::parse(server, Some(endpoint))
                            }
                            _ => None,
                        }
                    }
                }
            }
        };
        let location = location.ok_or_else(|| {
            format!(
                "Fabric didn't return the server of {} (connector '{connector}'); check that the \
                 item exists and has finished provisioning.",
                item.describe()
            )
        })?;
        self.store(item, &location);
        Ok(location)
    }

    async fn get(&self, token: &str, item: &Item<'_>, path: &str) -> Result<Value, String> {
        let connector = item.connector;
        let url = self
            .base
            .join(path)
            .map_err(|e| format!("Bad Fabric API path {path}: {e}"))?;
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|error| {
                format!(
                    "Couldn't reach the Fabric API to find the database of connector \
                     '{connector}' ({error}). Check your network connection, then try again."
                )
            })?;
        let status = response.status();
        if status.is_success() {
            return response.json().await.map_err(|error| {
                format!(
                    "The Fabric API answered unreadable JSON for connector '{connector}': {error}."
                )
            });
        }
        let what = item.describe();
        Err(match status {
            StatusCode::UNAUTHORIZED => format!(
                "Fabric refused your Azure token while looking up {what} (connector \
                 '{connector}'). Run `az login` with the tenant of the workspace, then try again."
            ),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => format!(
                "Fabric didn't find {what} for connector '{connector}' ({status}). Check \
                 config.workspaceId and config.itemId in rayfin/rayfin.yml, and that your account \
                 can open the item in Fabric (ask the workspace's admin for access)."
            ),
            _ => format!(
                "The Fabric API answered {status} while looking up {what} for connector \
                 '{connector}'. Try again in a moment."
            ),
        })
    }
}

/// Writes `bytes` to `path` through a temporary file next to it.
fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_parse_from_fabric_connection_strings() {
        let ado = "Data Source=abc.database.fabric.microsoft.com,1433;Initial Catalog=Db-1;\
                   Multiple Active Result Sets=False;Encrypt=True";
        assert_eq!(
            Location::parse(ado, None),
            Some(Location {
                host: "abc.database.fabric.microsoft.com".into(),
                port: 1433,
                database: "Db-1".into(),
            })
        );
        let fqdn = Location::parse("abc.database.fabric.microsoft.com,1433", Some("Db")).unwrap();
        assert_eq!((fqdn.port, fqdn.database.as_str()), (1433, "Db"));
        let host = Location::parse("x.datawarehouse.fabric.microsoft.com", Some("id")).unwrap();
        assert_eq!(host.port, 1433);
        assert_eq!(
            Location::parse("tcp:h,14330", Some("d")).unwrap().port,
            14330
        );
        assert_eq!(Location::parse("x.fabric.microsoft.com", None), None);
        assert_eq!(Location::parse("", Some("d")), None);
    }

    #[test]
    fn locations_are_cached_per_item() {
        let dir = tempfile::tempdir().unwrap();
        let locator = Locator::new(
            "https://api.fabric.microsoft.com",
            "scope".into(),
            Some(dir.path().to_path_buf()),
        )
        .unwrap();
        let item = Item {
            connector: "sales",
            kind: SqlKind::Warehouse,
            workspace: "w",
            id: "i",
        };
        let location = Location::parse("h,1433", Some("i")).unwrap();
        locator.store(&item, &location);
        assert_eq!(locator.cached(&item), Some(location));
        let other = Item { id: "j", ..item };
        assert_eq!(locator.cached(&other), None);
        locator.forget("sales");
        assert_eq!(locator.cached(&item), None);
    }
}
