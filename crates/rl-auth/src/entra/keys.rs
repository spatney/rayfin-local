//! Microsoft Entra's token signing keys: downloaded from the public discovery endpoints, cached
//! by key id, refreshed daily and when a token names an unknown key (at most once a minute, so
//! made-up key ids cannot hammer Microsoft).

use std::{
    collections::HashMap,
    sync::OnceLock,
    time::{Duration, Instant},
};

use parking_lot::RwLock;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use serde_json::Value;

use crate::util::b64url_decode;

/// The v2 list first: it marks the keys that only one tenant may use (`issuer`).
const MICROSOFT_KEY_URLS: [&str; 2] = [
    "https://login.microsoftonline.com/common/discovery/v2.0/keys",
    "https://login.microsoftonline.com/common/discovery/keys",
];
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// An RSA public key from a JWKS.
#[derive(Clone)]
pub(crate) struct SigningKey {
    n: Vec<u8>,
    e: Vec<u8>,
    /// The only tenant whose tokens the key may sign (`issuer` names a tenant), lowercase.
    pub(crate) tenant: Option<String>,
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKey")
            .field("bits", &(self.n.len() * 8))
            .field("tenant", &self.tenant)
            .finish()
    }
}

impl SigningKey {
    /// Verifies an RS256 (RSASSA-PKCS1-v1_5 with SHA-256) signature.
    pub(crate) fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        RsaPublicKeyComponents {
            n: &self.n,
            e: &self.e,
        }
        .verify(&RSA_PKCS1_2048_8192_SHA256, message, signature)
        .is_ok()
    }
}

/// Why no key could be found.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum KeyError {
    #[error("the token is signed with a key that is not one of Microsoft's (kid {0})")]
    Unknown(String),
    /// The keys could not be downloaded; the message says why and what to check.
    #[error("{0}")]
    Unavailable(String),
}

pub(crate) struct EntraKeys {
    /// Empty for a fixed key set (tests).
    urls: Vec<String>,
    client: OnceLock<reqwest::Client>,
    cache: RwLock<Cache>,
    /// Single flight: one download at a time.
    refreshing: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Cache {
    keys: HashMap<String, SigningKey>,
    fetched_at: Option<Instant>,
    attempted_at: Option<Instant>,
    /// Why the last download failed, when it did.
    error: Option<String>,
}

impl EntraKeys {
    /// Microsoft's public-cloud keys.
    pub(crate) fn microsoft() -> Self {
        Self::from_urls(MICROSOFT_KEY_URLS.map(str::to_owned).to_vec())
    }

    /// Keys downloaded from `urls` (JWKS documents), merged; the first document wins for a key
    /// id that several list.
    pub(crate) fn from_urls(urls: Vec<String>) -> Self {
        Self {
            urls,
            client: OnceLock::new(),
            cache: RwLock::default(),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    /// A fixed key set (a JWKS document), never refreshed.
    #[cfg(test)]
    pub(crate) fn fixed(jwks: &Value) -> Self {
        let keys = Self::from_urls(Vec::new());
        {
            let mut cache = keys.cache.write();
            cache.keys = parse_jwks(jwks).into_iter().collect();
            cache.fetched_at = Some(Instant::now());
        }
        keys
    }

    /// The key with id `kid`, downloading the key sets when the cache is old or lacks it.
    pub(crate) async fn key(&self, kid: &str) -> Result<SigningKey, KeyError> {
        if let Some(key) = self.fresh(kid) {
            return Ok(key);
        }
        let _refreshing = self.refreshing.lock().await;
        if let Some(key) = self.fresh(kid) {
            return Ok(key);
        }
        let due = self
            .cache
            .read()
            .attempted_at
            .is_none_or(|at| at.elapsed() >= MIN_REFRESH_INTERVAL);
        if due && !self.urls.is_empty() {
            self.download().await;
        }
        let cache = self.cache.read();
        // A key past its age still beats none when Microsoft can't be reached.
        if let Some(key) = cache.keys.get(kid) {
            return Ok(key.clone());
        }
        match &cache.error {
            Some(error) => Err(KeyError::Unavailable(error.clone())),
            None => Err(KeyError::Unknown(kid.to_owned())),
        }
    }

    fn fresh(&self, kid: &str) -> Option<SigningKey> {
        let cache = self.cache.read();
        let fresh =
            self.urls.is_empty() || cache.fetched_at.is_some_and(|at| at.elapsed() < MAX_AGE);
        if fresh {
            cache.keys.get(kid).cloned()
        } else {
            None
        }
    }

    async fn download(&self) {
        self.cache.write().attempted_at = Some(Instant::now());
        let client = match self.client() {
            Ok(client) => client,
            Err(error) => {
                self.cache.write().error = Some(error);
                return;
            }
        };
        let mut keys = HashMap::new();
        let mut failures = Vec::new();
        for url in &self.urls {
            match fetch_jwks(&client, url).await {
                Ok(found) => {
                    for (kid, key) in found {
                        keys.entry(kid).or_insert(key);
                    }
                }
                Err(error) => failures.push(format!("{url}: {error}")),
            }
        }
        let mut cache = self.cache.write();
        if keys.is_empty() {
            cache.error = Some(format!(
                "could not download Microsoft's token signing keys ({}). Check the network \
                 connection and proxy settings (HTTPS_PROXY), then retry",
                failures.join("; ")
            ));
            return;
        }
        if !failures.is_empty() {
            tracing::debug!(failures = %failures.join("; "), "some Entra key sets failed to download");
        }
        tracing::debug!(
            count = keys.len(),
            "downloaded Microsoft's token signing keys"
        );
        cache.keys = keys;
        cache.fetched_at = Some(Instant::now());
        cache.error = None;
    }

    fn client(&self) -> Result<reqwest::Client, String> {
        if let Some(client) = self.client.get() {
            return Ok(client.clone());
        }
        // reqwest panics without a crypto provider; the binary installs ring at startup.
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            return Err(
                "TLS is not set up in this process (no rustls crypto provider is \
                        installed), so Microsoft's token signing keys cannot be downloaded"
                    .to_owned(),
            );
        }
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent(concat!("rayfin-local/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("could not create an HTTP client: {error}"))?;
        Ok(self.client.get_or_init(|| client).clone())
    }
}

async fn fetch_jwks(
    client: &reqwest::Client,
    url: &str,
) -> Result<Vec<(String, SigningKey)>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| error.to_string())?;
    let document: Value = response.json().await.map_err(|error| error.to_string())?;
    let keys = parse_jwks(&document);
    if keys.is_empty() {
        return Err("the document lists no RSA signing keys".to_owned());
    }
    Ok(keys)
}

/// The RSA signing keys of a JWKS document.
fn parse_jwks(document: &Value) -> Vec<(String, SigningKey)> {
    let Some(keys) = document.get("keys").and_then(Value::as_array) else {
        return Vec::new();
    };
    keys.iter()
        .filter_map(|jwk| {
            let text = |name: &str| jwk.get(name).and_then(Value::as_str);
            if text("kty") != Some("RSA") || text("use").is_some_and(|use_| use_ != "sig") {
                return None;
            }
            let tenant = match text("issuer") {
                None => None,
                Some(issuer) => Some(issuer_tenant(issuer)?),
            };
            let key = SigningKey {
                n: decode_unsigned(text("n")?)?,
                e: decode_unsigned(text("e")?)?,
                tenant: tenant.flatten(),
            };
            Some((text("kid")?.to_owned(), key))
        })
        .collect()
}

/// `Some(None)` for the multi-tenant template (`…/{tenantid}/v2.0`), `Some(Some(tenant))` for a
/// key only that tenant may use, `None` for an issuer this code does not understand (the key is
/// then not trusted).
fn issuer_tenant(issuer: &str) -> Option<Option<String>> {
    let tenant = issuer
        .strip_prefix("https://login.microsoftonline.com/")?
        .strip_suffix("/v2.0")?;
    if tenant == "{tenantid}" {
        Some(None)
    } else if !tenant.is_empty()
        && tenant
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        Some(Some(tenant.to_ascii_lowercase()))
    } else {
        None
    }
}

/// A base64url big-endian integer (padding tolerated), without leading zero bytes.
fn decode_unsigned(text: &str) -> Option<Vec<u8>> {
    let bytes = b64url_decode(text.trim_end_matches('='))?;
    let start = bytes.iter().position(|b| *b != 0)?;
    Some(bytes[start..].to_vec())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use axum::{Json, Router, routing::get};
    use serde_json::json;

    use super::*;
    use crate::entra::tests::{OTHER_KID, TRUSTED_KID, jwk};

    #[test]
    fn keys_pinned_to_a_tenant_keep_their_tenant() {
        let mut pinned = jwk(TRUSTED_KID);
        pinned["issuer"] =
            json!("https://login.microsoftonline.com/9188040D-6C67-4C5B-B112-36A304B66DAD/v2.0");
        let mut template = jwk(OTHER_KID);
        template["issuer"] = json!("https://login.microsoftonline.com/{tenantid}/v2.0");
        let mut unknown_issuer = jwk("strange");
        unknown_issuer["issuer"] = json!("https://elsewhere.example/");
        let mut encryption = jwk("enc");
        encryption["use"] = json!("enc");
        let keys: HashMap<_, _> = parse_jwks(
            &json!({ "keys": [pinned, template, unknown_issuer, encryption, { "kty": "EC" }] }),
        )
        .into_iter()
        .collect();
        assert_eq!(keys.len(), 2);
        assert_eq!(
            keys[TRUSTED_KID].tenant.as_deref(),
            Some("9188040d-6c67-4c5b-b112-36a304b66dad")
        );
        assert_eq!(keys[OTHER_KID].tenant, None);
    }

    /// Serves `jwks` and counts the downloads.
    async fn jwks_server(jwks: Value) -> (String, Arc<AtomicUsize>) {
        let downloads = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&downloads);
        let app = Router::new().route(
            "/keys",
            get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                let jwks = jwks.clone();
                async move { Json(jwks) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/keys", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, downloads)
    }

    #[tokio::test]
    async fn keys_are_downloaded_once_and_unknown_ids_refresh_at_most_once_a_minute() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (url, downloads) = jwks_server(json!({ "keys": [jwk(TRUSTED_KID)] })).await;
        let keys = EntraKeys::from_urls(vec![url]);

        assert!(keys.key(TRUSTED_KID).await.is_ok());
        assert!(keys.key(TRUSTED_KID).await.is_ok());
        assert_eq!(downloads.load(Ordering::SeqCst), 1);

        assert_eq!(
            keys.key("made-up").await.unwrap_err(),
            KeyError::Unknown("made-up".into())
        );
        assert_eq!(
            downloads.load(Ordering::SeqCst),
            1,
            "a refresh within a minute of the last one is skipped"
        );
        keys.cache.write().attempted_at = Some(Instant::now() - MIN_REFRESH_INTERVAL);
        assert!(keys.key("made-up").await.is_err());
        assert_eq!(downloads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn unreachable_key_sets_are_reported_as_unavailable() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/keys", listener.local_addr().unwrap());
        drop(listener);
        let keys = EntraKeys::from_urls(vec![url]);
        let error = keys.key(TRUSTED_KID).await.unwrap_err();
        assert!(
            matches!(&error, KeyError::Unavailable(message) if message.contains("HTTPS_PROXY")),
            "{error}"
        );
    }
}
