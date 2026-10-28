//! The auth tables (`_rl_auth_*`), evolved with `Db::migrate_component`.
//!
//! Steps are append-only: never edit or reorder a released step, add a new one.

pub(crate) const COMPONENT: &str = "auth";

pub(crate) const STEPS: &[&str] = &[
    // 1: users, sessions (refresh-token families) and refresh tokens.
    "CREATE TABLE _rl_auth_users (
        id TEXT PRIMARY KEY NOT NULL,
        username TEXT NOT NULL,
        -- The case-folded username: usernames are unique case-insensitively.
        username_key TEXT NOT NULL UNIQUE,
        email TEXT,
        display_name TEXT,
        -- An argon2id PHC string; NULL for users who sign in with Entra only.
        password_hash TEXT,
        entra_oid TEXT UNIQUE,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    ) STRICT;

    -- A sign-in: one refresh-token family. user_id has no foreign key, so a deleted user's
    -- revoked sessions stay known until their access tokens have expired.
    CREATE TABLE _rl_auth_sessions (
        id TEXT PRIMARY KEY NOT NULL,
        user_id TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        -- Sliding expiry: the last refresh plus the refresh-token lifetime.
        expires_at INTEGER NOT NULL,
        last_refresh_at INTEGER NOT NULL,
        revoked_at INTEGER
    ) STRICT;
    CREATE INDEX _rl_auth_sessions_by_user ON _rl_auth_sessions (user_id);

    -- Refresh tokens are `<selector>.<verifier>`; only the verifier's SHA-256 is stored.
    CREATE TABLE _rl_auth_refresh_tokens (
        selector TEXT PRIMARY KEY NOT NULL,
        verifier_hash BLOB NOT NULL,
        session_id TEXT NOT NULL REFERENCES _rl_auth_sessions (id) ON DELETE CASCADE,
        issued_at INTEGER NOT NULL,
        -- Set when a newer token of the session replaced this one.
        rotated_at INTEGER
    ) STRICT;
    CREATE INDEX _rl_auth_refresh_tokens_by_session ON _rl_auth_refresh_tokens (session_id);",
    // 2: the sign-in portal's handoff codes (`codeType: fabric_handoff`).
    "CREATE TABLE _rl_auth_handoff_codes (
        -- Only the code's SHA-256 is stored.
        code_hash BLOB PRIMARY KEY NOT NULL,
        user_id TEXT NOT NULL REFERENCES _rl_auth_users (id) ON DELETE CASCADE,
        -- PKCE S256: base64url(SHA-256(code_verifier)) of the redeemer's verifier.
        code_challenge TEXT NOT NULL,
        -- The origin the code was sent to; the redeemer's redirectUri must have it.
        return_origin TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        used_at INTEGER
    ) STRICT;
    CREATE INDEX _rl_auth_handoff_codes_by_expiry ON _rl_auth_handoff_codes (expires_at);",
];
