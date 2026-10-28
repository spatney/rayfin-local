//! `services.auth` from `rayfin.yml`, as the auth service uses it.

use serde_json::{Map, Value, json};

use crate::AuthError;

/// Claims a static custom claim may not replace: the service needs them to verify its tokens.
const RESERVED_CLAIMS: &[&str] = &["sub", "sid", "iss", "aud", "exp", "nbf", "iat", "jti"];

/// The auth settings of the app, filled by the binary from `rayfin.yml` `services.auth`.
///
/// [`AuthSettings::new`] gives the real service's defaults. Out-of-range lifetimes are clamped
/// the way the real service clamps them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthSettings {
    /// `services.auth.enabled`. When `false`, every auth route answers 404 as the real service does.
    pub enabled: bool,
    /// The app (project) id: the access token's `aud`.
    pub app_id: String,
    /// The access token's `iss`. The real service uses `https://<project id>.rayfin.io`.
    pub issuer: String,
    /// `expiryInMinutes`: access-token lifetime; default 60, clamped to 15..=43200.
    pub access_token_minutes: u32,
    /// `refreshToken.lifetimeInDays`: sliding session lifetime; default 90, clamped to 1..=365.
    pub refresh_token_days: u32,
    /// `refreshToken.absoluteLifetimeInMinutes`: hard session limit from sign-in; `None` (the
    /// default, `-1` in `rayfin.yml`) means sliding only.
    pub session_absolute_lifetime_minutes: Option<u32>,
    /// `password.enabled`: sign-up and password reset; the SDK treats a missing value as `true`.
    pub password_enabled: bool,
    /// Local only: whether anyone may sign up (`POST /signup`). On by default.
    pub signup_enabled: bool,
    /// `fabric.enabled`: "Sign in with Fabric" (the broker popup).
    pub fabric_enabled: bool,
    /// `fabric.externalEntraExchange`: `POST /api/auth/v1/brokered/token`.
    pub external_entra_exchange: bool,
    /// `email.enabled`. Reported in the runtime settings only: there is no email locally.
    pub email_enabled: bool,
    /// `passwordless.magicLink.enabled`. Reported in the runtime settings only.
    pub magic_link_enabled: bool,
    /// `allowedRedirectUris`.
    pub allowed_redirect_uris: Vec<String>,
    /// `customClaims`: static claims added to every access token; `${NAME}` is replaced with
    /// the environment variable `NAME` when it is set.
    pub custom_claims: Vec<(String, String)>,
    /// `scopes`: joined with spaces into the `scope` claim and the token response's `scope`.
    pub scopes: Vec<String>,
}

impl AuthSettings {
    pub const DEFAULT_ACCESS_TOKEN_MINUTES: u32 = 60;
    pub const DEFAULT_REFRESH_TOKEN_DAYS: u32 = 90;
    const ACCESS_TOKEN_MINUTES: (u32, u32) = (15, 43_200);
    const REFRESH_TOKEN_DAYS: (u32, u32) = (1, 365);

    /// The real service's defaults for the app `app_id`.
    pub fn new(app_id: impl Into<String>) -> Self {
        let app_id = app_id.into();
        Self {
            enabled: true,
            issuer: Self::default_issuer(&app_id),
            app_id,
            access_token_minutes: Self::DEFAULT_ACCESS_TOKEN_MINUTES,
            refresh_token_days: Self::DEFAULT_REFRESH_TOKEN_DAYS,
            session_absolute_lifetime_minutes: None,
            password_enabled: true,
            signup_enabled: true,
            fabric_enabled: false,
            external_entra_exchange: false,
            email_enabled: false,
            magic_link_enabled: false,
            allowed_redirect_uris: Vec::new(),
            custom_claims: Vec::new(),
            scopes: Vec::new(),
        }
    }

    /// The issuer the real service uses for a project: `https://<project id>.rayfin.io`.
    pub fn default_issuer(app_id: &str) -> String {
        format!("https://{app_id}.rayfin.io")
    }

    /// Clamps lifetimes like the real service and drops custom claims that would break
    /// verification.
    pub(crate) fn normalized(mut self) -> Result<Self, AuthError> {
        if self.app_id.trim().is_empty() {
            return Err(AuthError::Settings(
                "the app id (the access tokens' audience) is empty".into(),
            ));
        }
        if self.issuer.trim().is_empty() {
            return Err(AuthError::Settings("the token issuer is empty".into()));
        }
        let (min, max) = Self::ACCESS_TOKEN_MINUTES;
        self.access_token_minutes = self.access_token_minutes.clamp(min, max);
        let (min, max) = Self::REFRESH_TOKEN_DAYS;
        self.refresh_token_days = self.refresh_token_days.clamp(min, max);
        self.session_absolute_lifetime_minutes =
            self.session_absolute_lifetime_minutes.map(|m| m.max(1));
        self.custom_claims.retain(|(name, _)| {
            let reserved = RESERVED_CLAIMS.contains(&name.as_str());
            if reserved {
                tracing::warn!(
                    claim = %name,
                    "services.auth.customClaims cannot replace a registered claim; ignoring it"
                );
            }
            !reserved
        });
        Ok(self)
    }

    /// The scopes joined as the `scope` claim and token-response field, `None` without scopes.
    pub(crate) fn scope(&self) -> Option<String> {
        (!self.scopes.is_empty()).then(|| self.scopes.join(" "))
    }

    /// `GET /api/projectRuntimeSettings`: the real service's settings document, auth part.
    /// The SDK reads `serviceSettings.auth.{enabled, password, passwordless, fabric}`.
    pub fn runtime_settings(&self) -> Value {
        let custom_claims = if self.custom_claims.is_empty() {
            Value::Null
        } else {
            let claims: Map<String, Value> = self
                .custom_claims
                .iter()
                .map(|(name, value)| (name.clone(), Value::String(value.clone())))
                .collect();
            Value::Object(claims)
        };
        let scopes = if self.scopes.is_empty() {
            Value::Null
        } else {
            json!(self.scopes)
        };
        let email = if self.email_enabled {
            json!({ "enabled": true })
        } else {
            Value::Null
        };
        let absolute_lifetime = self.session_absolute_lifetime_minutes.map_or(-1, i64::from);
        json!({
            "serviceSettings": {
                "auth": {
                    "enabled": self.enabled,
                    "expiryInMinutes": self.access_token_minutes,
                    "customClaims": custom_claims,
                    "scopes": scopes,
                    "email": email,
                    "refreshToken": {
                        "lifetimeInDays": self.refresh_token_days,
                        "absoluteLifetimeInMinutes": absolute_lifetime,
                    },
                    "password": { "enabled": self.password_enabled },
                    "passwordless": { "magicLink": { "enabled": self.magic_link_enabled } },
                    "fabric": {
                        "enabled": self.fabric_enabled,
                        "externalEntraExchange": self.external_entra_exchange,
                    },
                    "allowedRedirectUris": self.allowed_redirect_uris,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetimes_are_clamped_like_the_real_service() {
        let mut settings = AuthSettings::new("app");
        settings.access_token_minutes = 1;
        settings.refresh_token_days = 1000;
        settings.session_absolute_lifetime_minutes = Some(0);
        let settings = settings.normalized().unwrap();
        assert_eq!(settings.access_token_minutes, 15);
        assert_eq!(settings.refresh_token_days, 365);
        assert_eq!(settings.session_absolute_lifetime_minutes, Some(1));
    }

    #[test]
    fn custom_claims_cannot_replace_registered_claims() {
        let mut settings = AuthSettings::new("app");
        settings.custom_claims = vec![
            ("sub".into(), "admin".into()),
            ("app_version".into(), "1.0.0".into()),
        ];
        let settings = settings.normalized().unwrap();
        assert_eq!(
            settings.custom_claims,
            vec![("app_version".to_owned(), "1.0.0".to_owned())]
        );
    }

    #[test]
    fn an_empty_app_id_is_refused() {
        assert!(matches!(
            AuthSettings::new(" ").normalized(),
            Err(AuthError::Settings(_))
        ));
    }
}
