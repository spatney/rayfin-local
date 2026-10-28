//! [`TokenBroker`]: env overrides, then the Azure CLI and/or the Rayfin CLI's login, behind a
//! per-scope cache with single-flight fetches.

use std::{
    collections::HashMap,
    ffi::OsString,
    fmt,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, SystemTime},
};

use tokio::sync::watch;

use crate::{
    AccessToken, Audience, BoxFuture, TokenError, TokenSource, az::AzCli, jwt,
    rayfin_cli::RayfinCli,
};

/// A cached token is reused while it has more than this left (the Azure CLI and MSAL renew
/// theirs at the same mark).
const MIN_REMAINING: Duration = Duration::from_secs(5 * 60);

/// How long an env override that isn't a JWT is assumed to stay valid.
const ENV_TOKEN_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// Where tokens come from (`--token-source`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TokenMode {
    /// The Azure CLI, then the Rayfin CLI's login.
    #[default]
    Auto,
    /// Only `az account get-access-token`.
    Az,
    /// Only the Rayfin CLI's login (`rayfin login`).
    Rayfin,
}

impl TokenMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenMode::Auto => "auto",
            TokenMode::Az => "az",
            TokenMode::Rayfin => "rayfin",
        }
    }
}

impl fmt::Display for TokenMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TokenMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(TokenMode::Auto),
            "az" => Ok(TokenMode::Az),
            "rayfin" => Ok(TokenMode::Rayfin),
            other => Err(format!(
                "unknown token source `{other}`: use auto, az or rayfin"
            )),
        }
    }
}

/// What the Rayfin CLI provider needs: Node.js, and the project whose `@microsoft/rayfin-cli`
/// (a devDependency of Rayfin projects) it signs in with.
#[derive(Clone, Debug)]
pub struct RayfinCliConfig {
    pub node: rl_node::NodeRuntime,
    /// The project root (with `package.json`); the helper runs there.
    pub project_root: PathBuf,
    /// `<project>/.rayfin-local/cache`, where the helper script is written.
    pub cache_dir: PathBuf,
}

/// How a [`TokenBroker`] gets tokens.
#[derive(Clone, Debug, Default)]
pub struct TokenBrokerConfig {
    pub mode: TokenMode,
    /// The Entra tenant to get tokens from (default: the one the CLI is signed in to).
    pub tenant: Option<String>,
    /// Scopes to use instead of [`Audience::default_scope`].
    pub scope_overrides: HashMap<Audience, String>,
    /// The Rayfin CLI provider. Without it, only the Azure CLI (and env overrides) are used.
    pub rayfin: Option<RayfinCliConfig>,
    /// The Azure CLI command: program and leading arguments (default: `az` from `PATH`).
    pub az_program: Option<Vec<OsString>>,
}

/// Who the developer's tokens belong to, from a Power BI token's claims. Keep it out of
/// info-level logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeveloperIdentity {
    /// The sign-in name (`upn`, else `preferred_username`, `unique_name` or `email`).
    pub upn: String,
    /// The Entra tenant id (`tid`).
    pub tenant_id: String,
    /// The Entra object id (`oid`).
    pub object_id: Option<String>,
    /// The display name (`name`).
    pub name: Option<String>,
}

/// The [`TokenSource`] for the developer's identity.
///
/// A token for an audience's scope comes from `RAYFIN_LOCAL_TOKEN_<AUDIENCE>` when that is set
/// (see [`Audience::token_env_var`]); otherwise from the providers [`TokenMode`] selects. Tokens
/// are cached per scope and reused while they have more than 5 minutes left; concurrent requests
/// for a scope share one fetch, and different scopes are fetched in parallel. A request for an
/// audience's default scope gets its override's scope when one is configured.
#[derive(Clone)]
pub struct TokenBroker {
    inner: Arc<Inner>,
}

struct Inner {
    mode: TokenMode,
    tenant: Option<String>,
    scope_overrides: HashMap<Audience, String>,
    /// `RAYFIN_LOCAL_TOKEN_<AUDIENCE>` values, read at startup.
    env_tokens: HashMap<Audience, String>,
    az: AzCli,
    rayfin: Option<RayfinCli>,
    cache: Mutex<HashMap<String, Slot>>,
}

/// The outcome of a fetch, once it is done.
type Fetch = watch::Receiver<Option<Result<AccessToken, TokenError>>>;

#[derive(Default)]
struct Slot {
    token: Option<AccessToken>,
    /// The fetch in progress, which concurrent requests for the scope wait for.
    inflight: Option<Fetch>,
}

impl TokenBroker {
    pub fn new(config: TokenBrokerConfig) -> Self {
        let env_tokens = Audience::ALL
            .into_iter()
            .filter_map(|audience| {
                let token = clean_token(&std::env::var(audience.token_env_var()).ok()?)?;
                tracing::info!(
                    "{} is set: {} tokens come from it",
                    audience.token_env_var(),
                    audience.udf_name()
                );
                Some((audience, token))
            })
            .collect();
        Self::with_env_tokens(config, env_tokens)
    }

    fn with_env_tokens(config: TokenBrokerConfig, env_tokens: HashMap<Audience, String>) -> Self {
        let not_blank = |value: &String| !value.trim().is_empty();
        Self {
            inner: Arc::new(Inner {
                mode: config.mode,
                tenant: config.tenant.filter(not_blank).map(|t| t.trim().to_owned()),
                scope_overrides: config
                    .scope_overrides
                    .into_iter()
                    .filter(|(_, scope)| not_blank(scope))
                    .map(|(audience, scope)| (audience, scope.trim().to_owned()))
                    .collect(),
                env_tokens,
                az: AzCli::new(config.az_program),
                rayfin: config.rayfin.map(RayfinCli::new),
                cache: Mutex::default(),
            }),
        }
    }

    pub fn mode(&self) -> TokenMode {
        self.inner.mode
    }

    /// The tenant tokens are requested from, when one is configured.
    pub fn tenant(&self) -> Option<&str> {
        self.inner.tenant.as_deref()
    }

    /// The scope tokens for `audience` are minted for: its override, else its default scope.
    pub fn scope_for(&self, audience: Audience) -> String {
        self.inner.scope_for(audience).to_owned()
    }

    /// Who the tokens belong to, read from a Power BI token (for `rayfin-local doctor` and the
    /// sign-in page).
    pub async fn whoami(&self) -> Result<DeveloperIdentity, TokenError> {
        let scope = self.scope_for(Audience::PowerBi);
        let token = self.get(&scope).await?;
        let anonymous = || TokenError {
            scope: scope.clone(),
            message: "the Power BI token does not say who it belongs to".to_owned(),
            hint: Some(
                "Sign in with a user account (`az login`), not a service principal or managed \
                 identity."
                    .to_owned(),
            ),
        };
        let claims = jwt::jwt_claims(&token.token).ok_or_else(anonymous)?;
        let claim = |name| jwt::claim(&claims, name).map(str::to_owned);
        let upn = ["upn", "preferred_username", "unique_name", "email"]
            .into_iter()
            .find_map(claim)
            .ok_or_else(anonymous)?;
        Ok(DeveloperIdentity {
            upn,
            tenant_id: claim("tid").ok_or_else(anonymous)?,
            object_id: claim("oid"),
            name: claim("name"),
        })
    }

    async fn get(&self, requested: &str) -> Result<AccessToken, TokenError> {
        let requested = requested.trim();
        if requested.is_empty() {
            return Err(TokenError {
                scope: String::new(),
                message: "a token was requested without a scope".to_owned(),
                hint: None,
            });
        }
        let (scope, audience) = self.inner.resolve(requested);
        if let Some(audience) = audience
            && let Some(token) = self.inner.env_token(audience)
        {
            tracing::debug!(scope = %scope, provider = "env", "using {}", audience.token_env_var());
            return Ok(token);
        }

        let (fetch, start) = {
            let mut cache = lock(&self.inner.cache);
            let slot = cache.entry(scope.clone()).or_default();
            if let Some(token) = &slot.token
                && is_fresh(token)
            {
                return Ok(token.clone());
            }
            match &slot.inflight {
                Some(fetch) => (fetch.clone(), None),
                None => {
                    let (done, fetch) = watch::channel(None);
                    slot.inflight = Some(fetch.clone());
                    (fetch, Some(done))
                }
            }
        };
        if let Some(done) = start {
            let inflight = Inflight {
                inner: Arc::clone(&self.inner),
                scope: scope.clone(),
            };
            // A task of its own, so a fetch that is under way (maybe an interactive sign-in)
            // completes and is cached even when the request that started it is cancelled.
            tokio::spawn(async move {
                let result = inflight.inner.fetch(&inflight.scope).await;
                inflight.complete(result.as_ref().ok());
                done.send_replace(Some(result));
            });
        }
        wait(fetch, &scope).await
    }
}

impl TokenSource for TokenBroker {
    fn token<'a>(&'a self, scope: &'a str) -> BoxFuture<'a, Result<AccessToken, TokenError>> {
        Box::pin(self.get(scope))
    }
}

impl fmt::Debug for TokenBroker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenBroker")
            .field("mode", &self.inner.mode)
            .field("tenant", &self.inner.tenant)
            .field("scope_overrides", &self.inner.scope_overrides)
            .finish_non_exhaustive()
    }
}

impl Inner {
    fn scope_for(&self, audience: Audience) -> &str {
        self.scope_overrides
            .get(&audience)
            .map_or(audience.default_scope(), String::as_str)
    }

    /// The scope to mint for `requested`, and the audience it belongs to.
    fn resolve(&self, requested: &str) -> (String, Option<Audience>) {
        if let Some(audience) = Audience::ALL
            .into_iter()
            .find(|audience| self.scope_for(*audience) == requested)
        {
            return (requested.to_owned(), Some(audience));
        }
        match Audience::ALL
            .into_iter()
            .find(|audience| audience.default_scope() == requested)
        {
            Some(audience) => (self.scope_for(audience).to_owned(), Some(audience)),
            None => (requested.to_owned(), None),
        }
    }

    fn env_token(&self, audience: Audience) -> Option<AccessToken> {
        let token = self.env_tokens.get(&audience)?;
        let now = SystemTime::now();
        let expires_at = jwt::expiry(token).unwrap_or(now + ENV_TOKEN_LIFETIME);
        if expires_at <= now {
            tracing::warn!(
                "{} holds an expired token; set a fresh one or unset it",
                audience.token_env_var()
            );
        }
        Some(AccessToken {
            token: token.clone(),
            expires_at,
        })
    }

    async fn fetch(&self, scope: &str) -> Result<AccessToken, TokenError> {
        let tenant = self.tenant.as_deref();
        tracing::debug!(scope, mode = %self.mode, tenant, "fetching a token");
        let (provider, result) = match self.mode {
            TokenMode::Az => ("az", self.az.token(scope, tenant).await),
            TokenMode::Rayfin => ("rayfin", self.rayfin_token(scope).await),
            TokenMode::Auto => match self.az.token(scope, tenant).await {
                Ok(token) => ("az", Ok(token)),
                Err(az_error) => match &self.rayfin {
                    None => ("az", Err(az_error)),
                    Some(rayfin) => {
                        tracing::info!(
                            "the Azure CLI could not get a token for {scope} ({}); trying the \
                             Rayfin CLI's login",
                            az_error.message
                        );
                        let result = rayfin.token(scope, tenant).await;
                        (
                            "rayfin",
                            result.map_err(|error| combine(scope, az_error, error)),
                        )
                    }
                },
            },
        };
        match &result {
            Ok(token) => tracing::debug!(
                scope,
                provider,
                expires_in_s = token
                    .expires_at
                    .duration_since(SystemTime::now())
                    .map_or(0, |left| left.as_secs()),
                "got a token"
            ),
            Err(error) => tracing::debug!(scope, provider, error = %error.message, "no token"),
        }
        result
    }

    async fn rayfin_token(&self, scope: &str) -> Result<AccessToken, TokenError> {
        match &self.rayfin {
            Some(rayfin) => rayfin.token(scope, self.tenant.as_deref()).await,
            None => Err(TokenError {
                scope: scope.to_owned(),
                message: "the Rayfin CLI's login is not available: it needs Node.js and the \
                          project's @microsoft/rayfin-cli"
                    .to_owned(),
                hint: Some(
                    "Install Node.js 20 or later, or use the Azure CLI (`--token-source az`)."
                        .to_owned(),
                ),
            }),
        }
    }
}

/// Ends a fetch: caches its token and clears the slot's in-flight marker, also when the fetch
/// panics (otherwise the scope would wait on a fetch that never finishes).
struct Inflight {
    inner: Arc<Inner>,
    scope: String,
}

impl Inflight {
    fn complete(self, token: Option<&AccessToken>) {
        if let Some(token) = token
            && let Some(slot) = lock(&self.inner.cache).get_mut(&self.scope)
        {
            slot.token = Some(token.clone());
        }
    }
}

impl Drop for Inflight {
    fn drop(&mut self) {
        if let Some(slot) = lock(&self.inner.cache).get_mut(&self.scope) {
            slot.inflight = None;
        }
    }
}

async fn wait(mut fetch: Fetch, scope: &str) -> Result<AccessToken, TokenError> {
    let failed = || TokenError {
        scope: scope.to_owned(),
        message: "the token request failed unexpectedly".to_owned(),
        hint: Some("Retry; run with --verbose to see the details.".to_owned()),
    };
    match fetch.wait_for(Option::is_some).await {
        Ok(result) => result.as_ref().cloned().unwrap_or_else(|| Err(failed())),
        Err(_) => Err(failed()),
    }
}

/// One error for both providers failing (auto mode).
fn combine(scope: &str, az: TokenError, rayfin: TokenError) -> TokenError {
    let hint = match (az.hint, rayfin.hint) {
        (Some(az), Some(rayfin)) => Some(format!("{az} Or, for the Rayfin CLI: {rayfin}")),
        (az, rayfin) => az.or(rayfin),
    };
    TokenError {
        scope: scope.to_owned(),
        message: format!(
            "neither the Azure CLI nor the Rayfin CLI's login could get a token for {scope}. \
             Azure CLI: {}. Rayfin CLI: {}",
            az.message.trim_end_matches('.'),
            rayfin.message.trim_end_matches('.')
        ),
        hint,
    }
}

fn is_fresh(token: &AccessToken) -> bool {
    token
        .expires_at
        .duration_since(SystemTime::now())
        .is_ok_and(|left| left > MIN_REMAINING)
}

/// An env override's token, without blanks or a `Bearer ` prefix.
fn clean_token(value: &str) -> Option<String> {
    let value = value.trim();
    let value = match value.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("bearer ") => value[7..].trim(),
        _ => value,
    };
    (!value.is_empty()).then(|| value.to_owned())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A broker whose Azure CLI can't run, so every token must come from `env`.
    fn broker(overrides: &[(Audience, &str)], env: &[(Audience, &str)]) -> TokenBroker {
        TokenBroker::with_env_tokens(
            TokenBrokerConfig {
                mode: TokenMode::Az,
                scope_overrides: overrides
                    .iter()
                    .map(|(audience, scope)| (*audience, scope.to_string()))
                    .collect(),
                az_program: Some(vec!["rayfin-local-test-no-such-az".into()]),
                ..TokenBrokerConfig::default()
            },
            env.iter()
                .map(|(audience, token)| (*audience, clean_token(token).unwrap()))
                .collect(),
        )
    }

    #[tokio::test]
    async fn env_overrides_win_for_their_audience_only() {
        let jwt = jwt::fake_jwt(&json!({ "aud": "499b84ac", "exp": 1_900_000_000 }));
        let broker = broker(
            &[],
            &[
                (Audience::Ado, &format!("Bearer {jwt}")),
                (Audience::PowerBi, "opaque"),
            ],
        );

        let ado = broker.token(Audience::Ado.default_scope()).await.unwrap();
        assert_eq!(ado.token, jwt);
        assert_eq!(
            ado.expires_at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_900_000_000)
        );

        let power_bi = broker
            .token(Audience::PowerBi.default_scope())
            .await
            .unwrap();
        assert_eq!(power_bi.token, "opaque");
        let left = power_bi
            .expires_at
            .duration_since(SystemTime::now())
            .unwrap();
        assert!(left > Duration::from_secs(59 * 60) && left <= ENV_TOKEN_LIFETIME);

        let fabric = broker
            .token(Audience::Fabric.default_scope())
            .await
            .unwrap_err();
        assert!(
            fabric.message.contains("not installed"),
            "{}",
            fabric.message
        );
    }

    #[tokio::test]
    async fn env_overrides_follow_scope_overrides() {
        let broker = broker(
            &[(Audience::Ado, "api://ado-proxy/.default")],
            &[(Audience::Ado, "t")],
        );
        assert_eq!(broker.scope_for(Audience::Ado), "api://ado-proxy/.default");
        for scope in ["api://ado-proxy/.default", Audience::Ado.default_scope()] {
            assert_eq!(broker.token(scope).await.unwrap().token, "t");
        }
    }
}
