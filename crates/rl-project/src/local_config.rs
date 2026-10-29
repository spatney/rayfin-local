//! `rayfin-local.toml`: optional rayfin-local settings at the project root.
//!
//! ```toml
//! [server]
//! port = 7171
//! host = "127.0.0.1"
//!
//! [frontend]
//! port = 5175
//!
//! [tokens]
//! source = "auto"        # "auto" | "az" | "rayfin"
//! tenant = "contoso.onmicrosoft.com"
//!
//! [tokens.scopes]        # audience → OAuth scope
//! AzureAI = "https://cognitiveservices.azure.com/.default"
//!
//! [connectors]
//! powerbi_base_url = "https://api.powerbi.com"
//!
//! [functions]
//! inspect_port = 9229
//!
//! [auth]
//! auto_create_users = true  # a password sign-in to an unknown email creates the account
//! ```
//!
//! Every setting is optional. Unknown keys are errors, since the file is rayfin-local's own.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ProjectError;
use crate::error::line_column;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalConfig {
    #[serde(default)]
    pub server: LocalServerConfig,
    #[serde(default)]
    pub frontend: LocalFrontendConfig,
    #[serde(default)]
    pub tokens: LocalTokensConfig,
    #[serde(default)]
    pub connectors: LocalConnectorsConfig,
    #[serde(default)]
    pub functions: LocalFunctionsConfig,
    #[serde(default)]
    pub auth: LocalAuthConfig,
}

/// `[auth]`: local sign-in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalAuthConfig {
    /// Create the account on a password sign-in to an unknown email (default on).
    pub auto_create_users: Option<bool>,
}

/// `[server]`: the backend.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalServerConfig {
    pub port: Option<u16>,
    pub host: Option<String>,
}

/// `[frontend]`: the app's dev server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalFrontendConfig {
    pub port: Option<u16>,
}

/// `[tokens]`: Azure tokens for the developer's identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalTokensConfig {
    pub source: Option<TokenSourceKind>,
    /// Entra tenant to request tokens from.
    pub tenant: Option<String>,
    /// Scope overrides by audience name (`ADO`, `AzureAI`, `Fabric`, `Sql`, `Storage`,
    /// `PowerBI`).
    #[serde(default)]
    pub scopes: BTreeMap<String, String>,
}

/// Where tokens come from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenSourceKind {
    /// The Azure CLI, then the Rayfin CLI's login.
    #[default]
    Auto,
    /// Only the Azure CLI.
    Az,
    /// Only the Rayfin CLI's login.
    Rayfin,
}

/// `[connectors]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalConnectorsConfig {
    /// Power BI REST base URL for semantic-model queries (tests, sovereign clouds).
    pub powerbi_base_url: Option<String>,
}

/// `[functions]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalFunctionsConfig {
    /// Start the function host with the Node inspector on this port.
    pub inspect_port: Option<u16>,
}

impl LocalConfig {
    pub const FILE_NAME: &str = "rayfin-local.toml";

    /// Reads `path`; a missing file gives the defaults.
    pub fn load(path: &Path) -> Result<Self, ProjectError> {
        match std::fs::read_to_string(path) {
            Ok(content) => Self::parse(path, &content),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ProjectError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Parses `content` (read from `path`).
    pub fn parse(path: &Path, content: &str) -> Result<Self, ProjectError> {
        let content = content.strip_prefix('\u{feff}').unwrap_or(content);
        let invalid = |offset: Option<usize>, message: String| {
            let (line, column) = offset.map(|o| line_column(content, o)).unzip();
            ProjectError::InvalidLocalConfig {
                path: path.to_path_buf(),
                line,
                column,
                message,
            }
        };
        let config: LocalConfig = toml::from_str(content)
            .map_err(|err| invalid(err.span().map(|span| span.start), err.message().to_string()))?;

        let key_offset = |key: &str| content.find(key);
        let blank = |value: &Option<String>| value.as_deref().is_some_and(|v| v.trim().is_empty());
        if blank(&config.server.host) {
            return Err(invalid(
                key_offset("host"),
                "server.host must not be empty".into(),
            ));
        }
        if blank(&config.tokens.tenant) {
            return Err(invalid(
                key_offset("tenant"),
                "tokens.tenant must not be empty".into(),
            ));
        }
        if let Some((audience, _)) = config
            .tokens
            .scopes
            .iter()
            .find(|(_, s)| s.trim().is_empty())
        {
            return Err(invalid(
                key_offset(audience),
                format!("tokens.scopes.{audience} must not be empty"),
            ));
        }
        if let Some(url) = &config.connectors.powerbi_base_url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            return Err(invalid(
                key_offset("powerbi_base_url"),
                format!("connectors.powerbi_base_url must be an http(s) URL, not '{url}'"),
            ));
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> Result<LocalConfig, ProjectError> {
        LocalConfig::parse(Path::new("rayfin-local.toml"), content)
    }

    #[test]
    fn reads_every_setting() {
        let config = parse(
            "[server]\nport = 7200\nhost = \"0.0.0.0\"\n[frontend]\nport = 5180\n[tokens]\n\
             source = \"az\"\ntenant = \"t\"\n[tokens.scopes]\nAzureAI = \"s/.default\"\n\
             [connectors]\npowerbi_base_url = \"http://127.0.0.1:9000\"\n[functions]\n\
             inspect_port = 9230\n[auth]\nauto_create_users = false\n",
        )
        .unwrap();
        assert_eq!(config.server.port, Some(7200));
        assert_eq!(config.server.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(config.frontend.port, Some(5180));
        assert_eq!(config.tokens.source, Some(TokenSourceKind::Az));
        assert_eq!(config.tokens.scopes["AzureAI"], "s/.default");
        assert_eq!(
            config.connectors.powerbi_base_url.as_deref(),
            Some("http://127.0.0.1:9000")
        );
        assert_eq!(config.functions.inspect_port, Some(9230));
        assert_eq!(config.auth.auto_create_users, Some(false));
        assert_eq!(parse("").unwrap(), LocalConfig::default());
    }

    #[test]
    fn rejects_unknown_keys_with_their_name_and_position() {
        let err = parse("[server]\nport = 1\nhots = \"x\"\n").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("(line 3, column 1)"), "{message}");
        assert!(message.contains("unknown field `hots`"), "{message}");
        assert!(parse("[tokens]\nsource = \"msal\"\n").is_err());
        assert!(
            parse("[database]\npath = \"x\"\n")
                .unwrap_err()
                .to_string()
                .contains("database")
        );
    }

    #[test]
    fn validates_values() {
        let err = parse("[connectors]\npowerbi_base_url = \"api.powerbi.com\"\n").unwrap_err();
        assert!(err.to_string().contains("(line 2, column 1)"), "{err}");
        assert!(parse("[tokens.scopes]\nADO = \" \"\n").is_err());
        assert!(parse("[server]\nport = 70000\n").is_err());
    }
}
