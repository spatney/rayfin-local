//! Minting the tokens a function's `ctx.Tokens` exposes.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use rl_tokens::{Audience, TokenError, TokenSource};
use tokio::{sync::watch, time::Instant};

/// How long minting one token may take; the function then runs without it.
const MINT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long an audience whose token could not be had is not tried again: each invocation would
/// otherwise wait for the same failure (a failing `az` run, an unanswered interactive sign-in).
const FAILURE_TTL: Duration = Duration::from_secs(30);

/// How long an invocation waits for a token another invocation is getting, before it runs
/// without it.
const JOIN_WAIT: Duration = Duration::from_secs(3);

/// A failing audience is reported at most this often.
const WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Mints a token for each audience, in parallel. An audience whose token can't be had is left out
/// (reading it in the function throws), with a warning.
pub(super) async fn mint(
    source: &dyn TokenSource,
    audiences: &[Audience],
    function: &str,
    fetches: &TokenFetches,
) -> Vec<(Audience, String)> {
    let tokens = audiences
        .iter()
        .map(|&audience| async move { (audience, fetches.token(source, audience).await) });
    let mut minted = Vec::with_capacity(audiences.len());
    for (audience, result) in futures_util::future::join_all(tokens).await {
        match result {
            Ok(token) => minted.push((audience, token)),
            Err(error) => {
                if fetches.should_warn(audience, Instant::now()) {
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

/// The token fetches of each audience, shared by the invocations: one at a time, and none for
/// [`FAILURE_TTL`] after one failed.
#[derive(Default)]
pub(super) struct TokenFetches {
    audiences: Mutex<HashMap<Audience, AudienceFetches>>,
}

#[derive(Default)]
struct AudienceFetches {
    /// Ends (its sender is dropped) when the fetch in flight does.
    in_flight: Option<watch::Receiver<()>>,
    /// The last fetch's error, and when it failed.
    failure: Option<(Instant, TokenError)>,
    /// When a failure was last reported.
    warned: Option<Instant>,
}

/// What an invocation does to get an audience's token.
enum Next<'a> {
    Fetch(Fetch<'a>),
    /// Waits for another invocation's fetch, then looks again.
    Wait(watch::Receiver<()>),
    Fail(TokenError),
}

/// The fetch in flight for an audience; it ends, waking those waiting for it, when dropped.
struct Fetch<'a> {
    fetches: &'a TokenFetches,
    audience: Audience,
    _done: watch::Sender<()>,
}

impl Drop for Fetch<'_> {
    fn drop(&mut self) {
        if let Some(state) = self.fetches.lock().get_mut(&self.audience) {
            state.in_flight = None;
        }
    }
}

impl TokenFetches {
    /// A token for `audience`, fetched from `source`. The error of a fetch that failed less than
    /// [`FAILURE_TTL`] ago is returned again; while another invocation fetches it, this one waits
    /// up to [`JOIN_WAIT`] for that fetch.
    async fn token(
        &self,
        source: &dyn TokenSource,
        audience: Audience,
    ) -> Result<String, TokenError> {
        let scope = audience.default_scope();
        let join_deadline = Instant::now() + JOIN_WAIT;
        loop {
            let mut in_flight = match self.next(audience, Instant::now()) {
                Next::Fail(error) => return Err(error),
                Next::Wait(in_flight) => in_flight,
                Next::Fetch(fetch) => {
                    let result = match tokio::time::timeout(MINT_TIMEOUT, source.token(scope)).await
                    {
                        Ok(result) => result.map(|token| token.token),
                        Err(_) => Err(TokenError {
                            scope: scope.to_owned(),
                            message: format!("getting a token took longer than {MINT_TIMEOUT:?}"),
                            hint: None,
                        }),
                    };
                    let failure = result.as_ref().err().map(|e| (Instant::now(), e.clone()));
                    if let Some(state) = self.lock().get_mut(&audience) {
                        state.failure = failure;
                    }
                    drop(fetch);
                    return result;
                }
            };
            // Only the end of the fetch (its sender dropped) wakes this.
            if tokio::time::timeout_at(join_deadline, in_flight.changed())
                .await
                .is_err()
            {
                return Err(TokenError {
                    scope: scope.to_owned(),
                    message: format!(
                        "another invocation has been getting it for more than {JOIN_WAIT:?}"
                    ),
                    hint: None,
                });
            }
        }
    }

    fn next(&self, audience: Audience, now: Instant) -> Next<'_> {
        let mut audiences = self.lock();
        let state = audiences.entry(audience).or_default();
        if let Some((failed, error)) = &state.failure
            && now.saturating_duration_since(*failed) < FAILURE_TTL
        {
            return Next::Fail(error.clone());
        }
        if let Some(in_flight) = &state.in_flight {
            return Next::Wait(in_flight.clone());
        }
        let (done, in_flight) = watch::channel(());
        state.in_flight = Some(in_flight);
        Next::Fetch(Fetch {
            fetches: self,
            audience,
            _done: done,
        })
    }

    /// Whether a failure for `audience` at `now` is reported: the first one, then one a minute.
    fn should_warn(&self, audience: Audience, now: Instant) -> bool {
        let mut audiences = self.lock();
        let warned = &mut audiences.entry(audience).or_default().warned;
        match *warned {
            Some(at) if now.saturating_duration_since(at) < WARN_INTERVAL => false,
            _ => {
                *warned = Some(now);
                true
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Audience, AudienceFetches>> {
        self.audiences
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::future::BoxFuture;
    use rl_tokens::AccessToken;

    use super::*;

    /// Fails every fetch after `delay`, counting them.
    struct Failing {
        delay: Duration,
        fetches: AtomicUsize,
    }

    impl Failing {
        fn new(delay: Duration) -> Self {
            Self {
                delay,
                fetches: AtomicUsize::new(0),
            }
        }

        fn fetches(&self) -> usize {
            self.fetches.load(Ordering::SeqCst)
        }
    }

    impl TokenSource for Failing {
        fn token<'a>(&'a self, scope: &'a str) -> BoxFuture<'a, Result<AccessToken, TokenError>> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                tokio::time::sleep(self.delay).await;
                Err(TokenError {
                    scope: scope.to_owned(),
                    message: "not signed in".into(),
                    hint: Some("Run `az login`.".into()),
                })
            })
        }
    }

    #[test]
    fn failures_are_reported_once_a_minute_per_audience() {
        let fetches = TokenFetches::default();
        let start = Instant::now();
        assert!(fetches.should_warn(Audience::Ado, start));
        assert!(!fetches.should_warn(Audience::Ado, start + Duration::from_secs(59)));
        assert!(fetches.should_warn(Audience::Fabric, start + Duration::from_secs(1)));
        assert!(fetches.should_warn(Audience::Ado, start + WARN_INTERVAL));
        assert!(!fetches.should_warn(Audience::Ado, start + Duration::from_secs(61)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_fetch_is_not_tried_again_for_a_while() {
        let source = Failing::new(Duration::from_secs(1));
        let fetches = TokenFetches::default();
        let audiences = [Audience::Fabric];
        assert!(mint(&source, &audiences, "f", &fetches).await.is_empty());
        tokio::time::sleep(FAILURE_TTL - Duration::from_secs(2)).await;
        assert!(mint(&source, &audiences, "f", &fetches).await.is_empty());
        assert_eq!(source.fetches(), 1);

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(mint(&source, &audiences, "f", &fetches).await.is_empty());
        assert_eq!(source.fetches(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_fetch_in_flight_is_waited_for_briefly() {
        let source = Failing::new(MINT_TIMEOUT / 2);
        let fetches = TokenFetches::default();
        let audiences = [Audience::Fabric];
        let first = mint(&source, &audiences, "first", &fetches);
        let second = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let started = Instant::now();
            let minted = mint(&source, &audiences, "second", &fetches).await;
            (minted, started.elapsed())
        };
        let (first, (second, waited)) = tokio::join!(first, second);
        assert!(first.is_empty() && second.is_empty());
        assert!(
            waited >= JOIN_WAIT && waited < MINT_TIMEOUT / 2,
            "{waited:?}"
        );
        assert_eq!(source.fetches(), 1);
    }
}
