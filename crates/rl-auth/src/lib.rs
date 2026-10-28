//! Rayfin auth, locally: users with username/password, sessions as ES256 JWTs with rotating
//! refresh tokens, JWKS, the Fabric sign-in popup emulation (`ensureSignedInWithFabric`), and
//! the Entra token exchange. Implements [`rl_model::CallerResolver`] for the other routes.
//!
//! The wire contract is the real Rayfin auth service's, as the SDK 1.36.2 sees it: see
//! [`AuthService::router`] for the routes.

mod entra;
mod error;
mod handoff;
mod jwt;
mod keys;
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

use rl_db::Db;

pub use error::AuthError;
pub use routes::{JWKS_PATH, RUNTIME_SETTINGS_PATH};
pub use sessions::TokenResponse;
pub use settings::AuthSettings;
pub use users::User;

/// Routes the real service serves without checking `X-Publishable-Key`.
pub const PUBLISHABLE_KEY_EXEMPT_PATHS: &[&str] = &[JWKS_PATH, RUNTIME_SETTINGS_PATH];

/// The app's auth service: users, sessions and tokens over the project database.
pub struct AuthService {
    pub(crate) db: Db,
    pub(crate) settings: AuthSettings,
    pub(crate) jwt: Arc<jwt::Jwt>,
    pub(crate) revoked: sessions::RevokedSessions,
    /// Shifts the service's clock; tests use it to travel in time.
    clock_offset_ms: AtomicI64,
}

impl AuthService {
    /// Opens the auth service: brings its tables up to date, and loads the signing key from
    /// `keys_dir` (creating it on first run).
    pub async fn new(db: Db, keys_dir: PathBuf, settings: AuthSettings) -> Result<Self, AuthError> {
        let settings = settings.normalized()?;
        if settings.magic_link_enabled {
            tracing::warn!(
                "services.auth.passwordless.magicLink is enabled, but rayfin-local sends no email: \
                 magic links answer as if they were disabled"
            );
        }
        if settings.email_enabled {
            tracing::warn!(
                "services.auth.email is enabled, but rayfin-local sends no email: users are \
                 verified on sign-up and the email routes answer as if email were disabled"
            );
        }
        db.migrate_component(schema::COMPONENT, schema::STEPS)
            .await?;
        let key = tokio::task::spawn_blocking(move || keys::SigningKey::load_or_create(&keys_dir))
            .await??;
        let jwt = Arc::new(jwt::Jwt::new(key, &settings));
        let service = Self {
            db,
            settings,
            jwt,
            revoked: sessions::RevokedSessions::default(),
            clock_offset_ms: AtomicI64::new(0),
        };
        service.load_revoked_sessions().await?;
        Ok(service)
    }

    /// The settings in effect (lifetimes clamped).
    pub fn settings(&self) -> &AuthSettings {
        &self.settings
    }

    /// The JSON Web Key Set with the public signing key (`GET /.well-known/jwks.json`).
    pub fn jwks(&self) -> serde_json::Value {
        self.jwt.jwks()
    }

    pub(crate) fn now_ms(&self) -> i64 {
        util::unix_now_ms() + self.clock_offset_ms.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn advance_clock(&self, ms: i64) {
        self.clock_offset_ms.fetch_add(ms, Ordering::Relaxed);
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
