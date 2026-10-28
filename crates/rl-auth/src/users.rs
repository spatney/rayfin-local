//! Users: local username/password accounts (and, later, Entra identities), with argon2id
//! password hashes. The user-management API the CLI and the sign-in portal use lives here.

use std::fmt;

use argon2::{Argon2, PasswordHasher, PasswordVerifier, password_hash};
use rusqlite::{Connection, OptionalExtension, Row, params};
use time::OffsetDateTime;

use crate::{AuthError, AuthService, util::datetime_from_ms};

/// The longest username accepted (characters).
const MAX_USERNAME_CHARS: usize = 256;

/// An account of the app.
#[derive(Clone, PartialEq, Eq)]
pub struct User {
    /// A lowercase UUID: the token's `sub` (`claims.sub` in policies).
    pub id: String,
    /// What the user signs in with (the SDK sends it as `email`), as typed at sign-up.
    pub username: String,
    /// The `email` claim: the username when it is an email address, or the Entra email.
    pub email: Option<String>,
    /// The `name` claim when set (Entra users); otherwise the username is used.
    pub display_name: Option<String>,
    /// Whether the user can sign in with a password.
    pub has_password: bool,
    /// The Entra object id of users who signed in with Entra.
    pub entra_oid: Option<String>,
    pub created_at: OffsetDateTime,
}

impl User {
    /// The `name` claim.
    pub fn name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.username)
    }
}

impl fmt::Debug for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Identity details stay out of logs.
        f.debug_struct("User")
            .field("id", &self.id)
            .field("has_email", &self.email.is_some())
            .field("has_password", &self.has_password)
            .field("has_entra_oid", &self.entra_oid.is_some())
            .field("created_at", &self.created_at)
            .finish_non_exhaustive()
    }
}

impl AuthService {
    /// Creates a password user. The username is trimmed and unique case-insensitively; it
    /// becomes the `email` claim when it is an email address.
    pub async fn create_user(&self, username: &str, password: &str) -> Result<User, AuthError> {
        let username = validate_username(username)?.to_owned();
        validate_password(password)?;
        let key = username_key(&username);
        let exists = {
            let key = key.clone();
            self.db
                .call(move |conn| find_user(conn, "username_key = ?1", &key))
                .await??
                .is_some()
        };
        if exists {
            return Err(AuthError::UsernameTaken);
        }
        let password_hash = hash_password(password.to_owned()).await?;
        let email = looks_like_email(&username).then(|| username.clone());
        let now = self.now_ms();
        let user = NewUser {
            id: uuid::Uuid::new_v4().to_string(),
            username,
            username_key: key,
            email,
            display_name: None,
            password_hash: Some(password_hash),
            entra_oid: None,
            now,
        };
        self.db.call(move |conn| insert_user(conn, &user)).await?
    }

    /// The user with this username (case-insensitive) when `password` is theirs.
    pub async fn verify_password(&self, username: &str, password: &str) -> Result<User, AuthError> {
        let key = username_key(username.trim());
        let found = self
            .db
            .call(move |conn| find_user_with_hash(conn, &key))
            .await??;
        let Some((user, Some(hash))) = found else {
            return Err(AuthError::InvalidCredentials);
        };
        if verify_password_hash(password.to_owned(), hash).await? {
            Ok(user)
        } else {
            Err(AuthError::InvalidCredentials)
        }
    }

    /// Every user, oldest first.
    pub async fn list_users(&self) -> Result<Vec<User>, AuthError> {
        self.db
            .call(|conn| {
                let mut statement = conn.prepare(&format!(
                    "SELECT {USER_COLUMNS} FROM _rl_auth_users ORDER BY created_at, username_key"
                ))?;
                let users = statement
                    .query_map([], user_from_row)?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(users)
            })
            .await?
    }

    /// The user with this id, or else this username (case-insensitive).
    pub async fn find_user(&self, id_or_username: &str) -> Result<Option<User>, AuthError> {
        let id_or_username = id_or_username.trim().to_owned();
        self.db
            .call(move |conn| find_by_id_or_username(conn, &id_or_username))
            .await?
    }

    /// Deletes a user (by id or username) and signs out all their sessions.
    pub async fn delete_user(&self, id_or_username: &str) -> Result<User, AuthError> {
        let id_or_username = id_or_username.trim().to_owned();
        let now = self.now_ms();
        let (user, revoked) = self
            .db
            .call(move |conn| -> Result<_, AuthError> {
                let tx = conn.transaction()?;
                let user =
                    find_by_id_or_username(&tx, &id_or_username)?.ok_or(AuthError::UserNotFound)?;
                let revoked = crate::sessions::revoke_user_sessions(&tx, &user.id, now)?;
                tx.execute("DELETE FROM _rl_auth_users WHERE id = ?1", [&user.id])?;
                tx.commit()?;
                Ok((user, revoked))
            })
            .await??;
        self.forget_sessions(revoked, now);
        Ok(user)
    }

    /// Replaces a user's password (by id or username) and, as a password reset does on the real
    /// service, signs out all their sessions.
    pub async fn set_password(
        &self,
        id_or_username: &str,
        password: &str,
    ) -> Result<User, AuthError> {
        validate_password(password)?;
        let password_hash = hash_password(password.to_owned()).await?;
        let id_or_username = id_or_username.trim().to_owned();
        let now = self.now_ms();
        let (user, revoked) = self
            .db
            .call(move |conn| -> Result<_, AuthError> {
                let tx = conn.transaction()?;
                let user =
                    find_by_id_or_username(&tx, &id_or_username)?.ok_or(AuthError::UserNotFound)?;
                tx.execute(
                    "UPDATE _rl_auth_users SET password_hash = ?1, updated_at = ?2 WHERE id = ?3",
                    params![password_hash, now, user.id],
                )?;
                let revoked = crate::sessions::revoke_user_sessions(&tx, &user.id, now)?;
                tx.commit()?;
                Ok((
                    User {
                        has_password: true,
                        ..user
                    },
                    revoked,
                ))
            })
            .await??;
        self.forget_sessions(revoked, now);
        Ok(user)
    }

    /// The user for an Entra identity (`oid`), created on first sign-in. Email and display name
    /// are refreshed on later sign-ins.
    ///
    /// TODO(entra-exchange): the caller validates the Entra token; decide whether an Entra user
    /// may take over a password account with the same email (today they never merge).
    pub async fn find_or_create_external_user(
        &self,
        oid: &str,
        email: Option<&str>,
        display_name: Option<&str>,
    ) -> Result<User, AuthError> {
        let oid = oid.trim().to_owned();
        if oid.is_empty() {
            return Err(AuthError::InvalidInput("the Entra object id is empty"));
        }
        let email = email
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(str::to_owned);
        let display_name = display_name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned);
        let now = self.now_ms();
        self.db
            .call(move |conn| -> Result<User, AuthError> {
                let tx = conn.transaction()?;
                if let Some(user) = find_user(&tx, "entra_oid = ?1", &oid)? {
                    tx.execute(
                        "UPDATE _rl_auth_users SET email = ?1, display_name = ?2, updated_at = ?3
                         WHERE id = ?4",
                        params![email, display_name, now, user.id],
                    )?;
                    tx.commit()?;
                    return Ok(User {
                        email,
                        display_name,
                        ..user
                    });
                }
                let base = email.clone().unwrap_or_else(|| oid.clone());
                let username =
                    if find_user(&tx, "username_key = ?1", &username_key(&base))?.is_some() {
                        format!("{base} ({oid})")
                    } else {
                        base
                    };
                let user = insert_user(
                    &tx,
                    &NewUser {
                        id: uuid::Uuid::new_v4().to_string(),
                        username_key: username_key(&username),
                        username,
                        email,
                        display_name,
                        password_hash: None,
                        entra_oid: Some(oid),
                        now,
                    },
                )?;
                tx.commit()?;
                Ok(user)
            })
            .await?
    }
}

const USER_COLUMNS: &str =
    "id, username, email, display_name, password_hash IS NOT NULL, entra_oid, created_at";

fn user_from_row(row: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        username: row.get(1)?,
        email: row.get(2)?,
        display_name: row.get(3)?,
        has_password: row.get(4)?,
        entra_oid: row.get(5)?,
        created_at: datetime_from_ms(row.get(6)?),
    })
}

/// The user matching `condition` (with one parameter, `?1`).
pub(crate) fn find_user(
    conn: &Connection,
    condition: &str,
    value: &str,
) -> Result<Option<User>, AuthError> {
    let sql = format!("SELECT {USER_COLUMNS} FROM _rl_auth_users WHERE {condition}");
    Ok(conn.query_row(&sql, [value], user_from_row).optional()?)
}

fn find_user_with_hash(
    conn: &Connection,
    key: &str,
) -> Result<Option<(User, Option<String>)>, AuthError> {
    let sql =
        format!("SELECT {USER_COLUMNS}, password_hash FROM _rl_auth_users WHERE username_key = ?1");
    let found = conn
        .query_row(&sql, [key], |row| Ok((user_from_row(row)?, row.get(7)?)))
        .optional()?;
    Ok(found)
}

fn find_by_id_or_username(conn: &Connection, value: &str) -> Result<Option<User>, AuthError> {
    match find_user(conn, "id = ?1", &value.to_ascii_lowercase())? {
        Some(user) => Ok(Some(user)),
        None => find_user(conn, "username_key = ?1", &username_key(value)),
    }
}

struct NewUser {
    id: String,
    username: String,
    username_key: String,
    email: Option<String>,
    display_name: Option<String>,
    password_hash: Option<String>,
    entra_oid: Option<String>,
    now: i64,
}

fn insert_user(conn: &Connection, user: &NewUser) -> Result<User, AuthError> {
    let inserted = conn.execute(
        "INSERT INTO _rl_auth_users (id, username, username_key, email, display_name,
            password_hash, entra_oid, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        params![
            user.id,
            user.username,
            user.username_key,
            user.email,
            user.display_name,
            user.password_hash,
            user.entra_oid,
            user.now,
        ],
    );
    match inserted {
        Ok(_) => Ok(User {
            id: user.id.clone(),
            username: user.username.clone(),
            email: user.email.clone(),
            display_name: user.display_name.clone(),
            has_password: user.password_hash.is_some(),
            entra_oid: user.entra_oid.clone(),
            created_at: datetime_from_ms(user.now),
        }),
        // Another process took the username between the check and the insert.
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
        {
            Err(AuthError::UsernameTaken)
        }
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn username_key(username: &str) -> String {
    username.to_lowercase()
}

fn validate_username(username: &str) -> Result<&str, AuthError> {
    let username = username.trim();
    if username.is_empty() {
        return Err(AuthError::InvalidInput("Email is required"));
    }
    if username.chars().count() > MAX_USERNAME_CHARS {
        return Err(AuthError::InvalidInput(
            "Email is too long (at most 256 characters)",
        ));
    }
    if username.chars().any(char::is_control) {
        return Err(AuthError::InvalidInput(
            "Email cannot contain control characters",
        ));
    }
    Ok(username)
}

/// The real service only requires a non-blank password (the SDK asks for 6 characters at
/// sign-up, client-side).
fn validate_password(password: &str) -> Result<(), AuthError> {
    if password.trim().is_empty() {
        return Err(AuthError::InvalidInput(
            "Password is required and cannot be empty",
        ));
    }
    Ok(())
}

/// Whether `text` is a plausible email address (`local@domain.tld`), the bar for an `email` claim.
pub(crate) fn looks_like_email(text: &str) -> bool {
    let Some((local, domain)) = text.rsplit_once('@') else {
        return false;
    };
    let labels: Vec<&str> = domain.split('.').collect();
    !local.is_empty()
        && local.len() <= 64
        && text.len() <= 254
        && !local.contains('@')
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && !text.chars().any(|c| c.is_whitespace() || c.is_control())
        && labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_alphanumeric() || c == '-')
        })
}

async fn hash_password(password: String) -> Result<String, AuthError> {
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|hash| hash.to_string())
            .map_err(|error| AuthError::PasswordHash(error.to_string()))
    })
    .await?
}

async fn verify_password_hash(password: String, hash: String) -> Result<bool, AuthError> {
    tokio::task::spawn_blocking(move || {
        match Argon2::default().verify_password(password.as_bytes(), hash.as_str()) {
            Ok(()) => Ok(true),
            Err(password_hash::Error::PasswordInvalid) => Ok(false),
            Err(error) => Err(AuthError::PasswordHash(error.to_string())),
        }
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::service;

    #[test]
    fn email_detection_needs_a_dotted_domain() {
        assert!(looks_like_email("alice@example.test"));
        assert!(looks_like_email("Alice.Smith+tag@sub.example.co"));
        assert!(looks_like_email("zoë@exämple.de"));
        for not_email in [
            "alice",
            "alice@localhost",
            "@example.test",
            "alice@",
            "al ice@example.test",
            "a@b@example.test",
            "alice@example..test",
            "alice@-example.test",
        ] {
            assert!(!looks_like_email(not_email), "{not_email}");
        }
    }

    #[tokio::test]
    async fn usernames_are_unique_case_insensitively_and_trimmed() {
        let (_dir, auth) = service().await;
        let user = auth
            .create_user("  Alice@Example.test ", "secret1")
            .await
            .unwrap();
        assert_eq!(user.username, "Alice@Example.test");
        assert_eq!(user.email.as_deref(), Some("Alice@Example.test"));
        assert!(user.has_password);
        assert!(matches!(
            auth.create_user("alice@example.TEST", "secret2").await,
            Err(AuthError::UsernameTaken)
        ));
        let plain = auth.create_user("bob", "secret1").await.unwrap();
        assert_eq!(plain.email, None);
        let names: Vec<_> = auth
            .list_users()
            .await
            .unwrap()
            .into_iter()
            .map(|u| u.username)
            .collect();
        assert_eq!(names, ["Alice@Example.test", "bob"]);
    }

    #[tokio::test]
    async fn blank_usernames_and_passwords_are_refused() {
        let (_dir, auth) = service().await;
        assert!(matches!(
            auth.create_user("  ", "secret1").await,
            Err(AuthError::InvalidInput(_))
        ));
        assert!(matches!(
            auth.create_user("alice", "   ").await,
            Err(AuthError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn passwords_verify_case_insensitively_on_the_username_only() {
        let (_dir, auth) = service().await;
        let user = auth.create_user("alice", "Secret-1").await.unwrap();
        assert_eq!(
            auth.verify_password("ALICE", "Secret-1").await.unwrap(),
            user
        );
        assert!(matches!(
            auth.verify_password("alice", "secret-1").await,
            Err(AuthError::InvalidCredentials)
        ));
        assert!(matches!(
            auth.verify_password("nobody", "Secret-1").await,
            Err(AuthError::InvalidCredentials)
        ));
    }

    #[tokio::test]
    async fn set_password_replaces_the_password() {
        let (_dir, auth) = service().await;
        auth.create_user("alice", "old-secret").await.unwrap();
        auth.set_password("Alice", "new-secret").await.unwrap();
        assert!(auth.verify_password("alice", "new-secret").await.is_ok());
        assert!(auth.verify_password("alice", "old-secret").await.is_err());
        assert!(matches!(
            auth.set_password("nobody", "x-secret").await,
            Err(AuthError::UserNotFound)
        ));
    }

    #[tokio::test]
    async fn users_are_deleted_by_id_or_username() {
        let (_dir, auth) = service().await;
        let alice = auth.create_user("alice", "secret1").await.unwrap();
        let bob = auth.create_user("bob", "secret1").await.unwrap();
        assert_eq!(
            auth.delete_user(&alice.id.to_uppercase()).await.unwrap(),
            alice
        );
        assert_eq!(auth.delete_user("BOB").await.unwrap().id, bob.id);
        assert!(auth.list_users().await.unwrap().is_empty());
        assert!(matches!(
            auth.delete_user("alice").await,
            Err(AuthError::UserNotFound)
        ));
    }

    #[tokio::test]
    async fn external_users_are_keyed_by_oid() {
        let (_dir, auth) = service().await;
        auth.create_user("carol@contoso.com", "secret1")
            .await
            .unwrap();
        let first = auth
            .find_or_create_external_user("oid-1", Some("carol@contoso.com"), Some("Carol"))
            .await
            .unwrap();
        assert!(!first.has_password);
        assert_eq!(first.username, "carol@contoso.com (oid-1)");
        assert_eq!(first.name(), "Carol");
        let again = auth
            .find_or_create_external_user("oid-1", Some("carol@contoso.com"), Some("Carol C."))
            .await
            .unwrap();
        assert_eq!(again.id, first.id);
        assert_eq!(again.display_name.as_deref(), Some("Carol C."));
        assert!(matches!(
            auth.verify_password("carol@contoso.com (oid-1)", "").await,
            Err(AuthError::InvalidCredentials)
        ));
    }
}
