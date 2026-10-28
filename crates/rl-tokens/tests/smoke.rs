//! A manual check against this machine's real Azure CLI login:
//! `cargo test -p rl-tokens --test smoke -- --ignored --nocapture`.
//! It prints only expiries, lengths and audiences, never a token.

use std::time::SystemTime;

use rl_tokens::{Audience, TokenBroker, TokenBrokerConfig, TokenMode, TokenSource, jwt_claims};

#[tokio::test]
#[ignore = "needs a signed-in Azure CLI (`az login`)"]
async fn real_az_tokens() {
    let broker = TokenBroker::new(TokenBrokerConfig {
        mode: TokenMode::Az,
        ..TokenBrokerConfig::default()
    });
    let mut failed = Vec::new();
    for audience in [
        Audience::PowerBi,
        Audience::Ado,
        Audience::Fabric,
        Audience::AzureAi,
        Audience::Sql,
        Audience::Storage,
    ] {
        let scope = broker.scope_for(audience);
        match broker.token(&scope).await {
            Ok(token) => {
                let aud = jwt_claims(&token.token)
                    .and_then(|claims| claims.get("aud")?.as_str().map(str::to_owned));
                let left = token
                    .expires_at
                    .duration_since(SystemTime::now())
                    .map_or(0, |left| left.as_secs());
                println!(
                    "{audience:?} ({scope}): {} chars, aud {aud:?}, expires in {left} s",
                    token.token.len()
                );
            }
            Err(error) => {
                println!(
                    "{audience:?} ({scope}): {} | hint: {:?}",
                    error.message, error.hint
                );
                failed.push(audience);
            }
        }
    }
    match broker.whoami().await {
        Ok(me) => println!(
            "whoami: tenant {}, upn present: {}",
            me.tenant_id,
            !me.upn.is_empty()
        ),
        Err(error) => println!("whoami: {}", error.message),
    }
    assert!(
        !failed.contains(&Audience::PowerBi) && !failed.contains(&Audience::Ado),
        "no token for {failed:?}"
    );
}
