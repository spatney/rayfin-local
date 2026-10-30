//! Rayfin auth, locally: users with username/password, sessions as ES256 JWTs with rotating
//! refresh tokens, JWKS, the Fabric sign-in popup emulation (`ensureSignedInWithFabric`), a
//! local Fabric host for embedded apps (`initEmbeddedAuth`), and the Entra token exchange.
//! Implements [`rl_model::CallerResolver`] for the other routes.
//!
//! The wire contract is the real Rayfin auth service's, as the SDK 1.36.2 sees it: see
//! [`AuthService::router`] for the routes.

mod entra;
mod error;
mod handoff;
mod jwt;
mod keys;
mod portal;
mod resolver;
mod routes;
mod schema;
mod sessions;
mod settings;
mod users;
mod util;

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

use parking_lot::RwLock;
use rl_db::Db;

pub use error::AuthError;
pub use portal::{EMBED_HOST_PATH, PORTAL_PATH};
pub use routes::{JWKS_PATH, RUNTIME_SETTINGS_PATH};
pub use sessions::TokenResponse;
pub use settings::{AuthSettings, EntraSettings};
pub use users::User;

/// Routes the real service serves without checking `X-Publishable-Key`.
pub const PUBLISHABLE_KEY_EXEMPT_PATHS: &[&str] = &[JWKS_PATH, RUNTIME_SETTINGS_PATH];

/// The app's auth service: users, sessions and tokens over the project database.
pub struct AuthService {
    pub(crate) db: Db,
    /// The settings in effect; [`AuthService::set_settings`] replaces them.
    settings: RwLock<Arc<AuthSettings>>,
    /// Mints and verifies access tokens with the signing key and the settings in effect.
    jwt: RwLock<Arc<jwt::Jwt>>,
    pub(crate) revoked: Arc<sessions::RevokedSessions>,
    pub(crate) portal: portal::Portal,
    pub(crate) entra: entra::Entra,
    /// Shifts the service's clock; tests use it to travel in time.
    clock_offset_ms: AtomicI64,
}

impl AuthService {
    /// Opens the auth service: brings its tables up to date, and loads the signing key from
    /// `keys_dir` (creating it on first run). Until the service is dropped, a Tokio task reads the
    /// sign-outs other processes (the CLI) record in the database every 2 seconds.
    ///
    /// The Entra exchange downloads Microsoft's signing keys with `reqwest`: the process must
    /// install a rustls crypto provider first (the binary installs ring at startup).
    pub async fn new(db: Db, keys_dir: PathBuf, settings: AuthSettings) -> Result<Self, AuthError> {
        let settings = settings.normalized()?;
        warn_about_email(&settings, None);
        db.migrate_component(schema::COMPONENT, schema::STEPS)
            .await?;
        let key = tokio::task::spawn_blocking(move || keys::SigningKey::load_or_create(&keys_dir))
            .await??;
        let jwt = Arc::new(jwt::Jwt::new(key, &settings));
        let service = Self {
            settings: RwLock::new(Arc::new(settings)),
            jwt: RwLock::new(jwt),
            revoked: Arc::new(sessions::RevokedSessions::new(db.clone())),
            db,
            portal: portal::Portal::new()?,
            entra: entra::Entra::new(),
            clock_offset_ms: AtomicI64::new(0),
        };
        service.load_revoked_sessions().await?;
        service.watch_revoked_sessions();
        Ok(service)
    }

    /// Gives the sign-in page "Continue as <you>": `tokens` mints the developer's Power BI token
    /// (`rl_tokens::Audience::PowerBi`), whose claims say who they are. Only offered when
    /// [`EntraSettings::enabled`] is on.
    pub fn with_entra(mut self, tokens: Arc<dyn rl_tokens::TokenSource>) -> Self {
        self.entra.tokens = Some(tokens);
        self
    }

    /// The settings in effect (lifetimes clamped).
    pub fn settings(&self) -> Arc<AuthSettings> {
        Arc::clone(&self.settings.read())
    }

    /// Applies new settings while the service runs (`services.auth` of `rayfin.yml` changed).
    /// Users, sessions and the signing key stay; access tokens minted from now on get the new
    /// lifetime, claims and scopes, and the routes answer with the new modes at once.
    pub fn set_settings(&self, settings: AuthSettings) -> Result<(), AuthError> {
        let settings = settings.normalized()?;
        warn_about_email(&settings, Some(&self.settings()));
        let jwt = Arc::new(self.jwt().with_settings(&settings));
        *self.jwt.write() = jwt;
        *self.settings.write() = Arc::new(settings);
        Ok(())
    }

    /// Mints and verifies access tokens.
    pub(crate) fn jwt(&self) -> Arc<jwt::Jwt> {
        Arc::clone(&self.jwt.read())
    }

    /// The JSON Web Key Set with the public signing key (`GET /.well-known/jwks.json`).
    pub fn jwks(&self) -> serde_json::Value {
        self.jwt().jwks()
    }

    pub(crate) fn now_ms(&self) -> i64 {
        util::unix_now_ms() + self.clock_offset_ms.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn advance_clock(&self, ms: i64) {
        self.clock_offset_ms.fetch_add(ms, Ordering::Relaxed);
    }
}

/// Says that the email features `settings` turn on (and `previous` didn't) don't work locally.
fn warn_about_email(settings: &AuthSettings, previous: Option<&AuthSettings>) {
    if settings.magic_link_enabled && !previous.is_some_and(|previous| previous.magic_link_enabled)
    {
        tracing::warn!(
            "services.auth.passwordless.magicLink is enabled, but rayfin-local sends no email: \
             magic links answer as if they were disabled"
        );
    }
    if settings.email_enabled && !previous.is_some_and(|previous| previous.email_enabled) {
        tracing::warn!(
            "services.auth.email is enabled, but rayfin-local sends no email: users are \
             verified on sign-up and the email routes answer as if email were disabled"
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const APP_ID: &str = "6c6f6361-6c00-4000-8000-000000000001";

    /// A service over an in-memory database, with its key in a temporary folder.
    pub(crate) async fn service() -> (tempfile::TempDir, AuthService) {
        service_with(AuthSettings::new(APP_ID)).await
    }

    pub(crate) async fn service_with(settings: AuthSettings) -> (tempfile::TempDir, AuthService) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let auth = AuthService::new(db, dir.path().join("keys"), settings)
            .await
            .unwrap();
        (dir, auth)
    }
}
