//! Rayfin auth, locally: users with username/password, sessions as ES256 JWTs with rotating
//! refresh tokens, JWKS, the Fabric sign-in popup emulation (`ensureSignedInWithFabric`), and
//! the Entra token exchange. Implements [`rl_model::CallerResolver`] for the other routes.
