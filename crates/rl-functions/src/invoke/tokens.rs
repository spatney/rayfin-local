//! Minting the tokens a function's `ctx.Tokens` exposes.

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
    time::Duration,
};

use rl_tokens::{Audience, TokenError, TokenSource};
use tokio::time::Instant;

/// How long minting one token may take; the function then runs without it.
const MINT_TIMEOUT: Duration = Duration::from_secs(60);

/// A failing audience is reported at most this often.
const WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Mints a token for each audience, in parallel. An audience whose token can't be had is left out
/// (reading it in the function throws), with a warning.
pub(super) async fn mint(
    source: &dyn TokenSource,
    audiences: &[Audience],
    function: &str,
    warnings: &TokenWarnings,
) -> Vec<(Audience, String)> {
    let fetches = audiences.iter().map(|&audience| async move {
        let scope = audience.default_scope();
        let result = match tokio::time::timeout(MINT_TIMEOUT, source.token(scope)).await {
            Ok(result) => result,
            Err(_) => Err(TokenError {
                scope: scope.to_owned(),
                message: format!("getting a token took longer than {MINT_TIMEOUT:?}"),
                hint: None,
            }),
        };
        (audience, result)
    });
    let mut minted = Vec::with_capacity(audiences.len());
    for (audience, result) in futures_util::future::join_all(fetches).await {
        match result {
            Ok(token) => minted.push((audience, token.token)),
            Err(error) => {
                if warnings.should_warn(audience, Instant::now()) {
                    let hint = error
                        .hint
                        .as_deref()
                        .map(|h| format!(" {h}"))
                        .unwrap_or_default();
                    tracing::warn!(
                        "function '{function}' runs without ctx.Tokens.{}: {}.{hint}",
                        audience.udf_name(),
                        error.message.trim_end_matches('.'),
                    );
                } else {
                    tracing::debug!(
                        "function '{function}' runs without ctx.Tokens.{}: {}",
                        audience.udf_name(),
                        error.message
                    );
                }
            }
        }
    }
    minted
}

/// When each audience was last reported.
#[derive(Default)]
pub(super) struct TokenWarnings {
    last: Mutex<HashMap<Audience, Instant>>,
}

impl TokenWarnings {
    /// Whether a failure for `audience` at `now` is reported: the first one, then one a minute.
    fn should_warn(&self, audience: Audience, now: Instant) -> bool {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        match last.get(&audience) {
            Some(&at) if now.saturating_duration_since(at) < WARN_INTERVAL => false,
            _ => {
                last.insert(audience, now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_reported_once_a_minute_per_audience() {
        let warnings = TokenWarnings::default();
        let start = Instant::now();
        assert!(warnings.should_warn(Audience::Ado, start));
        assert!(!warnings.should_warn(Audience::Ado, start + Duration::from_secs(59)));
        assert!(warnings.should_warn(Audience::Fabric, start + Duration::from_secs(1)));
        assert!(warnings.should_warn(Audience::Ado, start + WARN_INTERVAL));
        assert!(!warnings.should_warn(Audience::Ado, start + Duration::from_secs(61)));
    }
}
