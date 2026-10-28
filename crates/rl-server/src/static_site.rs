//! `--static`: the built SPA served from the backend origin, with the SPA fallback to its index
//! document.

use std::path::{Component, Path, PathBuf};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use crate::StaticSite;
use crate::routes::{is_backend_path, json_error, not_found};

pub(crate) struct StaticFiles {
    dir: ServeDir,
    index: PathBuf,
    index_document: String,
}

impl StaticFiles {
    pub(crate) fn new(site: StaticSite) -> Self {
        let index_document = site.index_document.trim_start_matches(['/', '\\']);
        let index_document = if is_plain_relative(index_document) {
            index_document.to_owned()
        } else {
            tracing::warn!(
                index_document = %site.index_document,
                "the index document must be a path inside the static folder; using index.html"
            );
            "index.html".to_owned()
        };
        Self {
            // Directories have no listing or index of their own; they get the SPA fallback.
            dir: ServeDir::new(&site.root).append_index_html_on_directories(false),
            index: site.root.join(&index_document),
            index_document,
        }
    }

    /// Serves a request no route matched: a file under the root, else the index document for page
    /// navigations, else a JSON 404.
    pub(crate) async fn respond(&self, req: Request) -> Response {
        let method = req.method().clone();
        let path = req.uri().path().to_owned();
        if is_backend_path(&path) || (method != Method::GET && method != Method::HEAD) {
            return not_found(&method, &path);
        }
        let navigation = is_navigation(req.headers(), &path);
        let mut index_headers = req.headers().clone();

        // `ServeDir` rejects `..`, drive prefixes and Windows device names, so a request can't
        // leave the root.
        let file = self
            .dir
            .clone()
            .oneshot(req)
            .await
            .unwrap_or_else(|e| match e {});
        if file.status() != StatusCode::NOT_FOUND {
            return no_cache(file.map(Body::new));
        }
        if !navigation {
            return not_found(&method, &path);
        }

        index_headers.remove(header::RANGE);
        index_headers.remove(header::IF_RANGE);
        let mut index_req = Request::new(Body::empty());
        *index_req.method_mut() = method;
        *index_req.headers_mut() = index_headers;
        let index = ServeFile::new(&self.index)
            .oneshot(index_req)
            .await
            .unwrap_or_else(|e| match e {});
        if index.status() == StatusCode::NOT_FOUND {
            return json_error(
                StatusCode::NOT_FOUND,
                "not_found",
                &format!(
                    "The static site has no {}. Build the app (staticHosting.buildCommand) and \
                     restart `rayfin-local serve --static`.",
                    self.index_document
                ),
            );
        }
        no_cache(index.map(Body::new))
    }
}

fn is_plain_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(':')
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// Whether a request that matched no file should get the index document (client-side routing).
/// Yes when it has no `Accept` or lists `text/html` (page navigations). A bare wildcard (`*/*`,
/// sent by `fetch`, curl and script tags) counts only when the last segment has no extension, so a
/// missing `/assets/app.js` stays a 404 instead of becoming HTML.
fn is_navigation(headers: &HeaderMap, path: &str) -> bool {
    let Some(accept) = headers.get(header::ACCEPT) else {
        return true;
    };
    let Ok(accept) = accept.to_str() else {
        return false;
    };
    if accept.trim().is_empty() {
        return true;
    }
    let mut wildcard = false;
    for range in accept.split(',') {
        let mut parts = range.split(';');
        let media_type = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
        let refused = parts.any(|param| {
            let (name, value) = param.split_once('=').unwrap_or((param, ""));
            name.trim().eq_ignore_ascii_case("q")
                && value.trim().parse::<f32>().is_ok_and(|q| q <= 0.0)
        });
        if refused {
            continue;
        }
        match media_type.as_str() {
            "text/html" | "application/xhtml+xml" => return true,
            "*/*" | "text/*" => wildcard = true,
            _ => {}
        }
    }
    wildcard && !path.rsplit('/').next().unwrap_or_default().contains('.')
}

fn no_cache(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn navigations_get_the_index_and_assets_do_not() {
        let browser = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
        assert!(is_navigation(&accept(browser), "/threads/42"));
        assert!(is_navigation(&accept(browser), "/user/jane.doe"));
        assert!(is_navigation(&HeaderMap::new(), "/threads/42"));
        assert!(is_navigation(&accept("*/*"), "/threads/42"));
        assert!(is_navigation(&accept("*/*"), "/"));

        assert!(!is_navigation(&accept("*/*"), "/assets/missing.js"));
        assert!(!is_navigation(&accept("application/json"), "/threads/42"));
        assert!(!is_navigation(
            &accept("image/avif,image/webp,*/*;q=0.8"),
            "/logo.png"
        ));
        assert!(!is_navigation(
            &accept("text/html;q=0, application/json"),
            "/threads/42"
        ));
    }

    #[test]
    fn index_document_must_stay_inside_the_root() {
        assert!(is_plain_relative("index.html"));
        assert!(is_plain_relative("app/index.html"));
        assert!(!is_plain_relative("../index.html"));
        assert!(!is_plain_relative("C:/index.html"));
        assert!(!is_plain_relative(""));
    }
}
