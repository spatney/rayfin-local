//! Azure access tokens for the developer's own identity.
//!
//! Locally, rayfin-local stands in for both identities a deployed Rayfin app uses: the app
//! identity behind `ctx.Tokens.<Audience>` in functions, and the signed-in user behind delegated
//! connectors. Both become the developer, through the Azure CLI (`az account get-access-token`),
//! the Rayfin CLI's login (`@microsoft/rayfin-cli/auth`) or `RAYFIN_LOCAL_TOKEN_<AUDIENCE>` env
//! overrides. Consumers depend only on [`TokenSource`], so tests can use a fake; [`TokenBroker`]
//! is the real one.

mod az;
mod broker;
mod jwt;
mod rayfin_cli;

pub use broker::{DeveloperIdentity, RayfinCliConfig, TokenBroker, TokenBrokerConfig, TokenMode};
pub use jwt::jwt_claims;

use std::{fmt, future::Future, pin::Pin, time::SystemTime};

/// A boxed, sendable future (object-safe async trait methods).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An OAuth access token and when it expires.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken {
    pub token: String,
    pub expires_at: SystemTime,
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessToken")
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// A token could not be obtained: `message` says what failed, `hint` what the developer can do.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct TokenError {
    pub scope: String,
    pub message: String,
    pub hint: Option<String>,
}

/// Mints access tokens for OAuth scopes.
pub trait TokenSource: Send + Sync + 'static {
    /// A token for `scope` (for example `https://analysis.windows.net/powerbi/api/.default`) that
    /// stays valid for at least a few more minutes. Implementations cache.
    fn token<'a>(&'a self, scope: &'a str) -> BoxFuture<'a, Result<AccessToken, TokenError>>;
}

/// The resources rayfin-local mints tokens for: the `AudienceType` members functions declare on
/// `RayfinContext` (`@microsoft/fabric-user-data-functions` 1.36), and Power BI for
/// `fabric-semanticmodel` connectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Audience {
    Sql,
    Storage,
    Fabric,
    AzureAi,
    Ado,
    PowerBi,
}

impl Audience {
    /// The audiences a function can declare.
    pub const UDF: [Audience; 5] = [
        Audience::Sql,
        Audience::Storage,
        Audience::Fabric,
        Audience::AzureAi,
        Audience::Ado,
    ];

    /// Every audience: the UDF ones and Power BI.
    pub const ALL: [Audience; 6] = [
        Audience::Sql,
        Audience::Storage,
        Audience::Fabric,
        Audience::AzureAi,
        Audience::Ado,
        Audience::PowerBi,
    ];

    /// The env variable whose value, when set, is used as this audience's token:
    /// `RAYFIN_LOCAL_TOKEN_` and the UDF name in upper case (`RAYFIN_LOCAL_TOKEN_ADO`,
    /// `RAYFIN_LOCAL_TOKEN_POWERBI`, ...).
    pub fn token_env_var(self) -> String {
        format!(
            "RAYFIN_LOCAL_TOKEN_{}",
            self.udf_name().to_ascii_uppercase()
        )
    }

    /// The name the UDF package uses (`AudienceType.<name>`, `ctx.Tokens.<name>`, and
    /// `contextAudiences` in `runtimemetadata.json`).
    pub fn udf_name(self) -> &'static str {
        match self {
            Audience::Sql => "Sql",
            Audience::Storage => "Storage",
            Audience::Fabric => "Fabric",
            Audience::AzureAi => "AzureAI",
            Audience::Ado => "ADO",
            Audience::PowerBi => "PowerBI",
        }
    }

    /// Parses a UDF audience name, case-insensitively (`AI` is accepted for `AzureAI`).
    pub fn from_udf_name(name: &str) -> Option<Self> {
        let name = name.trim();
        [
            Audience::Sql,
            Audience::Storage,
            Audience::Fabric,
            Audience::AzureAi,
            Audience::Ado,
            Audience::PowerBi,
        ]
        .into_iter()
        .find(|a| a.udf_name().eq_ignore_ascii_case(name))
        .or_else(|| name.eq_ignore_ascii_case("AI").then_some(Audience::AzureAi))
    }

    /// The default OAuth scope for the audience (overridable in `rayfin-local.toml`).
    pub fn default_scope(self) -> &'static str {
        match self {
            Audience::Sql => "https://database.windows.net/.default",
            Audience::Storage => "https://storage.azure.com/.default",
            Audience::Fabric => "https://api.fabric.microsoft.com/.default",
            Audience::AzureAi => "https://ai.azure.com/.default",
            Audience::Ado => "499b84ac-1321-427f-aa17-267ca6975798/.default",
            Audience::PowerBi => "https://analysis.windows.net/powerbi/api/.default",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udf_names_round_trip() {
        for audience in Audience::UDF {
            assert_eq!(Audience::from_udf_name(audience.udf_name()), Some(audience));
        }
        assert_eq!(Audience::from_udf_name("ado"), Some(Audience::Ado));
        assert_eq!(Audience::from_udf_name("AI"), Some(Audience::AzureAi));
        assert_eq!(Audience::from_udf_name("Kusto"), None);
    }

    #[test]
    fn access_token_debug_hides_the_token() {
        let token = AccessToken {
            token: "eyJsecret".into(),
            expires_at: SystemTime::UNIX_EPOCH,
        };
        assert!(!format!("{token:?}").contains("eyJsecret"));
    }
}
