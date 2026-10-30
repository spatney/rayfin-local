//! The developer's Azure tokens as `serve` hands them to the feature services: the broker's,
//! with each audience whose token can't be had reported once to hosts (`azure-sign-in`).

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use rl_tokens::{AccessToken, Audience, BoxFuture, TokenBroker, TokenError, TokenSource};
use serde_json::json;

use crate::events::{self, WarningCode};

/// [`TokenBroker`]'s tokens, reporting failures as `azure-sign-in` warnings.
pub fn reporting(broker: Arc<TokenBroker>) -> Arc<dyn TokenSource> {
    Arc::new(ReportingTokens { broker })
}

struct ReportingTokens {
    broker: Arc<TokenBroker>,
}

impl ReportingTokens {
    /// The audience a scope belongs to: its default scope, or the one configured for it.
    fn audience(&self, scope: &str) -> Option<Audience> {
        let scope = scope.trim();
        Audience::ALL.into_iter().find(|audience| {
            audience.default_scope() == scope || self.broker.scope_for(*audience) == scope
        })
    }
}

impl TokenSource for ReportingTokens {
    fn token<'a>(&'a self, scope: &'a str) -> BoxFuture<'a, Result<AccessToken, TokenError>> {
        Box::pin(async move {
            let result = self.broker.token(scope).await;
            if let Err(err) = &result {
                report(self.audience(scope), scope, err);
            }
            result
        })
    }
}

/// An `azure-sign-in` warning, the first time an audience (or another scope) fails in this run.
fn report(audience: Option<Audience>, scope: &str, err: &TokenError) {
    static REPORTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let name = audience.map(Audience::udf_name);
    let key = name.unwrap_or(scope).to_owned();
    let first = REPORTED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key);
    if !first {
        return;
    }
    let what = name.map_or_else(|| format!("the scope {scope}"), str::to_owned);
    events::warning(
        WarningCode::AzureSignIn,
        format!(
            "No Azure token for {what}: {}.",
            err.message.trim_end_matches('.')
        ),
        err.hint.clone(),
        json!({ "audience": name, "scope": scope }),
    );
}
