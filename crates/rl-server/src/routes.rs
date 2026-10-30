//! Which paths belong to the backend, and the JSON error bodies the server answers itself.

use axum::Json;
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// Route roots the SDK calls with `X-Publishable-Key` (compat §2.2, §9.1, §9.2.2, §9.3).
const KEYED_ROOTS: [&str; 5] = [
    "/api",
    "/graphql",
    "/functions",
    "/connector-invoke",
    "/connectors",
];

/// Backend roots that are public: JWKS (compat §3.6) and rayfin-local's own pages, signed storage
/// URLs and health check.
const PUBLIC_ROOTS: [&str; 2] = ["/.well-known", "/.rayfin-local"];

/// rayfin-local's routes for programs on this machine, which browsers never call: a host's
/// control API and the dev server's automatic sign-in.
const PRIVATE_ROOTS: [&str; 2] = ["/.rayfin-local/control", "/.rayfin-local/dev"];

/// The JWKS alias under `/api` is public like `/.well-known/jwks.json` (compat §3.6).
const PUBLIC_UNDER_API: &str = "/api/auth/v1/.well-known";

/// Whether `path` is `root` or below it (`/api` matches `/api` and `/api/x`, not `/apiary`).
fn is_under(path: &str, root: &str) -> bool {
    path.strip_prefix(root)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

pub(crate) fn requires_publishable_key(path: &str) -> bool {
    KEYED_ROOTS.iter().any(|root| is_under(path, root)) && !is_under(path, PUBLIC_UNDER_API)
}

/// Whether `path` is one of the routes browsers never call, which CORS never allows.
pub(crate) fn is_private(path: &str) -> bool {
    PRIVATE_ROOTS.iter().any(|root| is_under(path, root))
}

/// Paths the static site never serves and never answers with the SPA's index document.
pub(crate) fn is_backend_path(path: &str) -> bool {
    KEYED_ROOTS
        .iter()
        .chain(PUBLIC_ROOTS.iter())
        .any(|root| is_under(path, root))
}

/// An error in the shape the SDK reads (compat §2.4): it prefers `error_description`, then `message`.
pub(crate) fn json_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "error": code,
        "error_description": message,
        "message": message,
    });
    (status, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

pub(crate) fn not_found(method: &Method, path: &str) -> Response {
    json_error(
        StatusCode::NOT_FOUND,
        "not_found",
        &format!("rayfin-local has no route for {method} {path}."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_match_whole_segments_only() {
        assert!(requires_publishable_key("/api"));
        assert!(requires_publishable_key("/api/storage/list"));
        assert!(requires_publishable_key("/graphql"));
        assert!(requires_publishable_key("/connectors/sales/graphql"));
        assert!(!requires_publishable_key("/apiary"));
        assert!(!requires_publishable_key("/graphql-playground"));
        assert!(!requires_publishable_key(
            "/api/auth/v1/.well-known/jwks.json"
        ));
        assert!(!requires_publishable_key("/.well-known/jwks.json"));

        assert!(is_backend_path("/.rayfin-local/portal/secureItemEmbed"));
        assert!(is_backend_path("/functions"));
        assert!(!is_backend_path("/function-list"));
        assert!(!is_backend_path("/"));
    }
}
