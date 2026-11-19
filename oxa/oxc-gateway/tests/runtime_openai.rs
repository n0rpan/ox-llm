#![cfg(all(feature = "anthropic-gateway", feature = "upstream-openai"))]
#[allow(dead_code)]
#[path = "common/anthropic.rs"]
mod anthropic;
#[allow(dead_code)]
#[path = "common/skeleton.rs"]
mod skeleton;
use oxc_gateway::{
    accounting::{Outcome, Usage},
    billing::SubscriptionLimits,
    catalog::{Access, Model, Provider},
    ledger::Context,
    pricing::Price,
    runtime::{Billing, OpenAiAuthHandle, Options, Runtime, Subscription},
};
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    time::Duration,
};

fn route(provider: Provider, upstream: &str) -> Model {
    Model::new(
        skeleton::MODEL,
        provider,
        upstream,
        "https://example.invalid",
        1000,
        100,
    )
    .unwrap()
}
fn payload() -> Value {
    json!({"model":skeleton::MODEL,"max_tokens":10,"messages":[]})
}
fn price(fixture: &skeleton::Fixture) -> Price {
    Price::from_model(&fixture.config["cloud"]["models"][skeleton::MODEL]).unwrap()
}
fn tokens(fixture: &skeleton::Fixture, access: &str) {
    let path = fixture.key_path.parent().unwrap().join("chatgpt-auth.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    write!(
        file,
        "{}",
        json!({"access_token":access,"refresh_token":"synthetic-refresh",
        "account_id":"synthetic-account","expires_at":2_000_000_000})
    )
    .unwrap();
}
fn subscription() -> Subscription {
    Subscription {
        refresh_endpoint: "https://example.invalid/token".into(),
        client_id: "synthetic-client-id".into(),
        timeout: Duration::from_secs(15),
    }
}
fn rows(fixture: &skeleton::Fixture) -> Vec<Value> {
    fs::read_dir(&fixture.ledger_dir)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            path.extension()
                .is_some_and(|s| s == "jsonl")
                .then(|| fs::read_to_string(path).unwrap())
        })
        .flat_map(|text| {
            text.lines()
                .map(|s| serde_json::from_str(s).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn api_admission_owns_the_fresh_key_and_uses_trusted_private_usage() {
    let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
    fs::write(
        &fixture.key_path,
        json!({"openai":{"api_key":anthropic::CREDENTIAL}}).to_string(),
    )
    .unwrap();
    let runtime = Runtime::new(
        [(
            route(Provider::OpenAi, "synthetic-upstream"),
            Some(Billing::Metered(price(&fixture))),
        )],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access: Access::Enabled,
            daily_budget_usd: Some(1.0),
            ..Options::default()
        },
    )
    .unwrap();
    let mut call = runtime.begin(&payload(), Context::default()).unwrap();
    let handle = call.openai_auth_handle().unwrap();
    let OpenAiAuthHandle::ApiKey(key) = &handle else {
        panic!("expected API key")
    };
    assert!(std::sync::Arc::ptr_eq(key, &call.key_handle().unwrap()));
    assert_eq!(format!("{handle:?}"), "OpenAiAuthHandle([redacted])");
    call.mark_forwarded().unwrap();
    call.metered_usage(Usage {
        prompt: Some(100),
        output: Some(5),
        cache_read: Some(10),
        cache_write: Some(0),
        ..Usage::default()
    });
    let row = call.finish(Outcome::Complete, 200, None).unwrap();
    assert_eq!(row["remote_provider"], "openai");
    assert_eq!(row["billing"], "metered");
    assert_eq!(row["settled"], "usage");
    assert_eq!(row["cost_usd"], json!(0.000197));
    assert!(!row.to_string().contains(anthropic::CREDENTIAL));
}

#[test]
fn subscription_admission_without_limits_is_not_refused_for_limits() {
    let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
    fs::write(
        fixture.key_path.parent().unwrap().join("chatgpt-auth.json"),
        "invalid synthetic JSON",
    )
    .unwrap();
    let runtime = Runtime::new(
        [(
            route(Provider::ChatGpt, "synthetic-upstream"),
            Some(Billing::Subscription),
        )],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access: Access::Enabled,
            ..Options::default()
        },
    )
    .unwrap();
    // No caps are configured: admission goes on to the credentials (invalid here) and the
    // refusal is about them, never about missing limits.
    let error = runtime.begin(&payload(), Context::default()).err().unwrap();
    assert_ne!(error.code, "subscription_limits_not_configured");
}

#[test]
fn subscription_owns_an_account_bound_handle_and_zero_usd_settlement_enforces_request_caps() {
    let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
    tokens(&fixture, "synthetic-access");
    let runtime = Runtime::new(
        [(
            route(Provider::ChatGpt, "synthetic-access-synthetic-refresh"),
            Some(Billing::Subscription),
        )],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access: Access::Enabled,
            subscription: Some(subscription()),
            subscription_limits: Some(SubscriptionLimits::new(1, 2000).unwrap()),
            ..Options::default()
        },
    )
    .unwrap();
    let mut call = runtime.begin(&payload(), Context::default()).unwrap();
    let handle = call.openai_auth_handle().unwrap();
    let OpenAiAuthHandle::Subscription {
        token_path,
        expected_account_id,
        ..
    } = &handle
    else {
        panic!("expected subscription")
    };
    assert_eq!(
        token_path,
        &fixture.key_path.parent().unwrap().join("chatgpt-auth.json")
    );
    assert_eq!(expected_account_id, "synthetic-account");
    assert_eq!(format!("{handle:?}"), "OpenAiAuthHandle([redacted])");
    assert!(call.key_handle().is_err());
    call.mark_forwarded().unwrap();
    call.metered_usage(Usage {
        prompt: Some(10),
        output: Some(5),
        cache_read: Some(2),
        ..Usage::default()
    });
    let row = call.finish(Outcome::Complete, 200, None).unwrap();
    assert_eq!(row["cost_usd"], 0.0);
    assert_eq!(row["subscription_requests"], 1);
    assert_eq!(row["subscription_tokens"], 15);
    assert_eq!(row["settled"], "usage");
    assert!(!row.to_string().contains("synthetic-access"));
    assert!(!row.to_string().contains("synthetic-refresh"));
    let error = runtime.begin(&payload(), Context::default()).err().unwrap();
    assert_eq!((error.status, error.code), (429, "subscription_limit_exceeded"));
}

#[test]
fn refreshing_a_bearer_does_not_change_the_stable_account_scope() {
    let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
    tokens(&fixture, "synthetic-access-before");
    let runtime = Runtime::new(
        [(
            route(Provider::ChatGpt, "synthetic-upstream"),
            Some(Billing::Subscription),
        )],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access: Access::Enabled,
            subscription: Some(subscription()),
            subscription_limits: Some(SubscriptionLimits::new(10, 10000).unwrap()),
            ..Options::default()
        },
    )
    .unwrap();
    let first = runtime.begin(&payload(), Context::default()).unwrap();
    let scope = first.provider_account_scope();
    first.finish(Outcome::Complete, 200, None).unwrap();
    tokens(&fixture, "synthetic-access-after");
    let second = runtime.begin(&payload(), Context::default()).unwrap();
    assert_eq!(scope, second.provider_account_scope());
    second.finish(Outcome::Complete, 200, None).unwrap();
}

#[test]
fn subscription_billing_cannot_be_selected_for_an_api_provider() {
    let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
    let error = Runtime::new(
        [(
            route(Provider::OpenAi, "synthetic-upstream"),
            Some(Billing::Subscription),
        )],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options::default(),
    )
    .err()
    .unwrap();
    assert_eq!((error.status, error.code), (400, "invalid_billing"));
    assert_eq!(fs::read_dir(&fixture.ledger_dir).unwrap().count(), 0);
}

fn go_runtime(fixture: &skeleton::Fixture, limits: Option<SubscriptionLimits>) -> Runtime {
    Runtime::new(
        [(
            route(Provider::OpenCode, "synthetic-go"),
            Some(Billing::Subscription),
        )],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access: Access::Enabled,
            subscription_limits: limits,
            ..Options::default()
        },
    )
    .unwrap()
}

#[test]
fn go_persists_usage_with_advisory_subscription_limits() {
    let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
    fs::write(
        &fixture.key_path,
        json!({"opencode":{"api_key":anthropic::CREDENTIAL}}).to_string(),
    )
    .unwrap();
    fs::write(
        fixture.key_path.parent().unwrap().join("chatgpt-auth.json"),
        "invalid synthetic JSON",
    )
    .unwrap();
    let limits = Some(SubscriptionLimits::new(1, 2000).unwrap());
    let runtime = go_runtime(&fixture, limits);
    assert_eq!(runtime.discovery().next().unwrap().provider, Provider::OpenCode);
    let mut call = runtime.begin(&payload(), Context::default()).unwrap();
    assert_eq!(call.key_handle().unwrap().expose(), anthropic::CREDENTIAL);
    assert!(call.openai_auth_handle().is_err());
    call.mark_forwarded().unwrap();
    call.metered_usage(Usage {
        prompt: Some(10),
        output: Some(5),
        reported_cost: Some(0.0),
        ..Usage::default()
    });
    let row = call.finish(Outcome::Complete, 200, None).unwrap();
    assert_eq!(row["remote_provider"], "opencode");
    assert_eq!(row["billing"], "subscription");
    assert_eq!(row["cost_usd"], 0.0);
    assert_eq!(row["subscription_requests"], 1);
    assert_eq!(row["subscription_tokens"], 15);
    drop(runtime);
    for limits in [limits, None] {
        let runtime = go_runtime(&fixture, limits);
        let mut call = runtime.begin(&payload(), Context::default()).unwrap();
        call.mark_forwarded().unwrap();
        call.metered_usage(Usage {
            prompt: Some(10),
            output: Some(5),
            reported_cost: Some(0.0),
            ..Usage::default()
        });
        let row = call.finish(Outcome::Complete, 200, None).unwrap();
        assert_eq!(row["subscription_requests"], 1);
        assert_eq!(row["subscription_tokens"], 15);
    }
    assert_eq!(rows(&fixture).len(), 3);
}

#[test]
fn a_malformed_go_key_is_refused_without_subscription_limits() {
    let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
    fs::write(&fixture.key_path, "invalid synthetic JSON").unwrap();
    let error = go_runtime(&fixture, None)
        .begin(&payload(), Context::default())
        .err()
        .unwrap();
    assert_eq!((error.status, error.code), (503, "cloud_credentials_unavailable"));
    let row = rows(&fixture).pop().unwrap();
    assert_eq!(row["status"], 503);
    assert_eq!(row["error"], "cloud_credentials_unavailable");
    assert_eq!(row["subscription_requests"], 0);
    assert_eq!(row["subscription_tokens"], 0);
}

#[test]
fn go_is_refused_to_api_and_guest_origins_before_credentials() {
    let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
    fs::write(&fixture.key_path, "invalid synthetic JSON").unwrap();
    let runtime = go_runtime(&fixture, Some(SubscriptionLimits::new(10, 20000).unwrap()));
    let error = runtime
        .begin(
            &payload(),
            Context {
                origin: oxc_gateway::ledger::Origin::Api,
                ..Context::default()
            },
        )
        .err()
        .unwrap();
    assert_eq!((error.status, error.code), (403, "cloud_provider_forbidden"));
    let row = rows(&fixture).pop().unwrap();
    assert_eq!(row["origin"], "api");
    assert_eq!(row["status"], 403);
    assert_eq!(row["error"], "cloud_provider_forbidden");
    assert_eq!(row["subscription_requests"], 0);
    assert_eq!(row["subscription_tokens"], 0);
}

#[test]
fn provider_session_hint_survives_restart_without_linking_providers_or_private_rows() {
    let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
    fs::write(
        &fixture.key_path,
        json!({"opencode":anthropic::CREDENTIAL,"openai":anthropic::CREDENTIAL}).to_string(),
    )
    .unwrap();
    let limits = Some(SubscriptionLimits::new(10, 20000).unwrap());
    let hint = |runtime: &Runtime, session: &str| {
        let call = runtime
            .begin(
                &payload(),
                Context {
                    session: Some(session),
                    ..Context::default()
                },
            )
            .unwrap();
        let hint = call.provider_session_hint().unwrap();
        let row = call.finish(Outcome::Complete, 200, None).unwrap();
        assert_ne!(hint, row["session"].as_str().unwrap());
        assert!(!hint.contains(session));
        assert_eq!(hint.len(), 32);
        assert!(hint.bytes().all(|b| b.is_ascii_hexdigit()));
        hint
    };
    let runtime = go_runtime(&fixture, limits);
    let first = hint(&runtime, "synthetic-conversation");
    assert_ne!(first, hint(&runtime, "another-conversation"));
    let call = runtime.begin(&payload(), Context::default()).unwrap();
    let error = call.provider_session_hint().unwrap_err();
    assert_eq!((error.status, error.code), (400, "invalid_value"));
    call.finish(Outcome::Complete, 200, None).unwrap();
    drop(runtime);
    assert_eq!(
        first,
        hint(&go_runtime(&fixture, limits), "synthetic-conversation")
    );
    let other = skeleton::Fixture::for_endpoint("https://example.invalid");
    fs::write(
        &other.key_path,
        json!({"opencode":anthropic::CREDENTIAL}).to_string(),
    )
    .unwrap();
    assert_ne!(first, hint(&go_runtime(&other, limits), "synthetic-conversation"));
    let api = Runtime::new(
        [(
            route(Provider::OpenAi, "synthetic-openai"),
            Some(Billing::Metered(price(&fixture))),
        )],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access: Access::Enabled,
            daily_budget_usd: Some(1.0),
            ..Options::default()
        },
    )
    .unwrap();
    assert_ne!(first, hint(&api, "synthetic-conversation"));
}

#[test]
fn authenticated_guest_context_refuses_every_cloud_provider_before_credentials() {
    for provider in [
        Provider::Anthropic,
        Provider::OpenAi,
        Provider::ChatGpt,
        Provider::OpenCode,
    ] {
        let fixture = skeleton::Fixture::for_endpoint("https://example.invalid");
        fs::write(&fixture.key_path, "invalid synthetic key JSON").unwrap();
        let billing = if matches!(provider, Provider::ChatGpt | Provider::OpenCode) {
            Billing::Subscription
        } else {
            Billing::Metered(price(&fixture))
        };
        let runtime = Runtime::new(
            [(route(provider, "synthetic-upstream"), Some(billing))],
            fixture.key_path.clone(),
            fixture.ledger_dir.clone(),
            Options {
                access: Access::Enabled,
                daily_budget_usd: Some(1.0),
                subscription_limits: Some(SubscriptionLimits::new(10, 20000).unwrap()),
                ..Options::default()
            },
        )
        .unwrap();
        let error = runtime
            .begin(
                &payload(),
                Context {
                    origin: oxc_gateway::ledger::Origin::Api,
                    guest_key_name: Some("synthetic-guest"),
                    ..Context::default()
                },
            )
            .err()
            .unwrap();
        assert_eq!((error.status, error.code), (403, "cloud_not_shared"));
        let row = rows(&fixture).pop().unwrap();
        assert_eq!(row["status"], 403);
        assert_eq!(row["error"], "cloud_not_shared");
        assert_eq!(row["cost_usd"], 0.0);
        assert!(!row.to_string().contains("synthetic-guest"));
    }
}
