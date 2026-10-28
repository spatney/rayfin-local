//! The Azure CLI provider and the broker's cache, against a fake `az`.

mod common;

use std::time::{Duration, SystemTime};

use common::FakeAz;
use rl_tokens::{
    Audience, DeveloperIdentity, TokenBroker, TokenBrokerConfig, TokenMode, TokenSource,
};
use serde_json::json;

const POWER_BI: &str = "https://analysis.windows.net/powerbi/api/.default";

#[tokio::test]
async fn reuses_a_token_until_it_is_about_to_expire() {
    let az = FakeAz::new();
    let broker = az.broker("ok");
    let first = broker.token(POWER_BI).await.unwrap();
    assert_eq!(broker.token(POWER_BI).await.unwrap(), first);
    assert_eq!(az.calls().len(), 1);
    let left = first.expires_at.duration_since(SystemTime::now()).unwrap();
    assert!(left > Duration::from_secs(3500) && left <= Duration::from_secs(3600));

    let az = FakeAz::new();
    let broker = az.broker("short-lived");
    let first = broker.token(POWER_BI).await.unwrap();
    assert_ne!(broker.token(POWER_BI).await.unwrap().token, first.token);
    assert_eq!(az.calls().len(), 2);
}

#[tokio::test]
async fn concurrent_requests_for_a_scope_share_one_az_run() {
    let az = FakeAz::new();
    let broker = az.broker("slow");
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let broker = broker.clone();
        requests.spawn(async move { broker.token(POWER_BI).await });
    }
    let tokens: Vec<_> = requests.join_all().await;
    assert!(tokens.iter().all(|token| token == &tokens[0]), "{tokens:?}");
    assert!(tokens[0].is_ok());
    assert_eq!(az.calls().len(), 1);
}

#[tokio::test]
async fn different_scopes_are_fetched_in_parallel() {
    let az = FakeAz::new();
    let broker = az.broker("slow");
    let (ado, fabric) = tokio::join!(
        broker.token(Audience::Ado.default_scope()),
        broker.token(Audience::Fabric.default_scope()),
    );
    let (ado, fabric) = (ado.unwrap(), fabric.unwrap());
    assert_ne!(ado.token, fabric.token);
    let calls = az.calls();
    assert_eq!(calls.len(), 2);
    let time = |call: usize, at: &str| calls[call][at].as_u64().unwrap();
    assert!(
        time(0, "start") < time(1, "end") && time(1, "start") < time(0, "end"),
        "the runs did not overlap: {calls:?}"
    );
}

#[tokio::test]
async fn passes_the_scope_and_tenant_to_az() {
    let az = FakeAz::new();
    let broker = az.broker_with(
        "ok",
        TokenBrokerConfig {
            tenant: Some("contoso.onmicrosoft.com".into()),
            ..TokenBrokerConfig::default()
        },
    );
    broker.token(POWER_BI).await.unwrap();
    assert_eq!(
        az.calls()[0]["args"],
        json!([
            "account",
            "get-access-token",
            "--scope",
            POWER_BI,
            "--output",
            "json",
            "--tenant",
            "contoso.onmicrosoft.com"
        ])
    );
}

#[tokio::test]
async fn scope_overrides_replace_an_audiences_default_scope() {
    let cognitive_services = "https://cognitiveservices.azure.com/.default";
    let az = FakeAz::new();
    let broker = az.broker_with(
        "ok",
        TokenBrokerConfig {
            scope_overrides: [(Audience::AzureAi, cognitive_services.to_owned())].into(),
            ..TokenBrokerConfig::default()
        },
    );
    assert_eq!(broker.scope_for(Audience::AzureAi), cognitive_services);
    assert_eq!(
        broker.scope_for(Audience::Ado),
        Audience::Ado.default_scope()
    );

    let by_default_scope = broker
        .token(Audience::AzureAi.default_scope())
        .await
        .unwrap();
    assert_eq!(
        broker.token(cognitive_services).await.unwrap(),
        by_default_scope
    );
    let calls = az.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["scope"], cognitive_services);
}

#[tokio::test]
async fn failures_carry_a_hint_and_are_not_cached() {
    let az = FakeAz::new();
    let broker = az.broker_with(
        "not-signed-in",
        TokenBrokerConfig {
            tenant: Some("contoso.onmicrosoft.com".into()),
            ..TokenBrokerConfig::default()
        },
    );
    let error = broker.token(POWER_BI).await.unwrap_err();
    assert_eq!(error.scope, POWER_BI);
    assert_eq!(
        error.message,
        "the Azure CLI is not signed in: Please run 'az login' to setup account."
    );
    assert_eq!(
        error.hint.as_deref(),
        Some("Run `az login --tenant contoso.onmicrosoft.com`, then retry.")
    );
    broker.token(POWER_BI).await.unwrap_err();
    assert_eq!(az.calls().len(), 2);
}

#[tokio::test]
async fn a_missing_az_says_how_to_install_it() {
    let broker = TokenBroker::new(TokenBrokerConfig {
        mode: TokenMode::Az,
        az_program: Some(vec![
            common::scratch_dir().path().join("no-such-az.exe").into(),
        ]),
        ..TokenBrokerConfig::default()
    });
    let error = broker.token(POWER_BI).await.unwrap_err();
    assert!(error.message.contains("not installed"), "{}", error.message);
    assert!(
        error
            .hint
            .unwrap()
            .contains("https://aka.ms/installazurecli")
    );
}

#[tokio::test]
async fn whoami_reads_the_power_bi_tokens_claims() {
    let az = FakeAz::new();
    assert_eq!(
        az.broker("user").whoami().await.unwrap(),
        DeveloperIdentity {
            upn: "dev@contoso.com".into(),
            tenant_id: "72f988bf-0000-0000-0000-000000000000".into(),
            object_id: Some("0b3c5e2a-0000-0000-0000-000000000000".into()),
            name: Some("Dev Eloper".into()),
        }
    );
}

/// `az` is `az.cmd` on Windows: its arguments go through `cmd.exe`.
#[cfg(windows)]
#[tokio::test]
async fn runs_a_cmd_shim_like_the_real_az() {
    let az = FakeAz::new();
    let shim_dir = common::scratch_dir();
    let shim = shim_dir.path().join("az.cmd");
    let quoted: Vec<String> = az
        .program("ok")
        .iter()
        .map(|arg| format!("\"{}\"", arg.to_string_lossy()))
        .collect();
    std::fs::write(&shim, format!("@{} %*\r\n", quoted.join(" "))).unwrap();
    let broker = TokenBroker::new(TokenBrokerConfig {
        mode: TokenMode::Az,
        tenant: Some("72f988bf-86f1-41af-91ab-2d7cd011db47".into()),
        az_program: Some(vec![shim.into()]),
        ..TokenBrokerConfig::default()
    });
    for audience in Audience::ALL {
        broker.token(audience.default_scope()).await.unwrap();
    }
    let mut scopes: Vec<String> = az
        .calls()
        .iter()
        .map(|call| call["scope"].as_str().unwrap().to_owned())
        .collect();
    scopes.sort();
    let mut expected: Vec<String> = Audience::ALL
        .iter()
        .map(|audience| audience.default_scope().to_owned())
        .collect();
    expected.sort();
    assert_eq!(scopes, expected);
    assert!(
        az.calls()
            .iter()
            .all(|call| call["tenant"] == "72f988bf-86f1-41af-91ab-2d7cd011db47")
    );
}
