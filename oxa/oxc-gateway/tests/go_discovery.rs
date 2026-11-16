#![cfg(feature = "anthropic-gateway")]
#[allow(dead_code)]
#[path = "common/anthropic.rs"]
mod anthropic;
#[allow(dead_code)]
#[path = "common/skeleton.rs"]
mod skeleton;
use anthropic::{MockAnthropic, Reply, CREDENTIAL};
use oxa_proto::CancelToken;
use oxc_gateway::{gateway, opencode::client, secrets};
use serde_json::json;
use std::time::Duration;

async fn discover(status: u16, body: &[u8]) -> Result<Vec<oxc_gateway::opencode::Model>, client::Error> {
    let mock = MockAnthropic::start(vec![Reply::wire("text/plain", body).with_status(status)]);
    let fixture = skeleton::Fixture::new(&mock);
    let key = secrets::read(&fixture.key_path, "anthropic").unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let result = client::models(
        &client,
        &mock.base_url(),
        &key,
        Duration::from_secs(3),
        &CancelToken::new(),
    )
    .await;
    let requests = mock.wait_for_requests(1);
    assert_eq!(requests[0].method, "GET");
    assert!(requests[0].target.ends_with("/v1/models"));
    mock.finish();
    result
}

#[tokio::test]
async fn discovery_http_refusals_do_not_require_json() {
    for (status, code) in [
        (401, "cloud_credentials_unavailable"),
        (403, "cloud_forbidden"),
        (429, "subscription_limit_reached"),
        (502, "cloud_upstream_error"),
    ] {
        for body in [
            b"<html>synthetic error page</html>".as_slice(),
            b"".as_slice(),
            br#"{"error":{"message":"synthetic error"}}"#.as_slice(),
        ] {
            let error = discover(status, body).await.unwrap_err();
            let refusal = gateway::Error::OpenCode(error).refusal();
            assert_eq!(refusal.code, code);
            assert!(!refusal.message.contains("synthetic"));
        }
    }
    let region = json!({"error":{"type":"invalid_request_error","message":"Model is hosted outside Europe and the United States"}});
    let error = discover(400, region.to_string().as_bytes()).await.unwrap_err();
    assert_eq!(gateway::Error::OpenCode(error).refusal().code, "region_blocked");
}

#[tokio::test]
async fn discovery_contract_failures_have_fixed_nonsecret_categories() {
    for (body, code) in [
        ("not JSON".to_owned(), "cloud_discovery_invalid_json"),
        (
            json!({"data":[],"hint":CREDENTIAL}).to_string(),
            "cloud_discovery_credential_echo",
        ),
        (
            json!({CREDENTIAL: true}).to_string(),
            "cloud_discovery_credential_echo",
        ),
        (json!({"data":{}}).to_string(), "cloud_discovery_catalog_shape"),
        (
            json!({"data":[{"limit":{"context":0,"output":1}}]}).to_string(),
            "cloud_discovery_model_limits",
        ),
    ] {
        let error = discover(200, body.as_bytes()).await.unwrap_err();
        let refusal = gateway::Error::OpenCode(error).refusal();
        assert_eq!(refusal.code, code);
        assert!(!refusal.message.contains(CREDENTIAL));
        assert!(!format!("{error:?}").contains(CREDENTIAL));
        assert!(!refusal.message.contains("http"));
    }
    assert_eq!(
        discover(200, json!({"data":[]}).to_string().as_bytes())
            .await
            .unwrap(),
        vec![]
    );
}

#[test]
fn discovery_startup_propagates_categories_without_inference_or_ledger_writes() {
    use oxc_gateway::{
        billing::SubscriptionLimits,
        catalog::Access,
        runtime::{Options, Runtime},
    };
    for (status, body, expected) in [
        (200, "not JSON", "cloud_discovery_invalid_json"),
        (200, "{}", "cloud_discovery_catalog_shape"),
        (
            401,
            "<html>synthetic refusal</html>",
            "cloud_credentials_unavailable",
        ),
    ] {
        let mock = MockAnthropic::start(vec![
            Reply::wire("text/plain", body.as_bytes()).with_status(status)
        ]);
        let fixture = skeleton::Fixture::new(&mock);
        std::fs::write(
            &fixture.key_path,
            json!({"opencode":{"api_key":CREDENTIAL}}).to_string(),
        )
        .unwrap();
        let runtime = Runtime::new(
            [],
            fixture.key_path.clone(),
            fixture.ledger_dir.clone(),
            Options {
                access: Access::Enabled,
                daily_budget_usd: None,
                ledger_enabled: true,
                subscription: None,
                subscription_limits: Some(SubscriptionLimits::new(10, 10000).unwrap()),
            },
        )
        .unwrap();
        let mut gateway = gateway::Gateway::new(runtime).unwrap();
        let error = gateway
            .discover_opencode(&mock.base_url(), Duration::from_secs(3))
            .unwrap_err();
        assert_eq!(error.refusal().code, expected);
        assert!(gateway.discovery().next().is_none());
        assert_eq!(std::fs::read_dir(&fixture.ledger_dir).unwrap().count(), 0);
        let requests = mock.wait_for_requests(1);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].target.ends_with("/v1/models"));
        mock.finish();
    }
}
