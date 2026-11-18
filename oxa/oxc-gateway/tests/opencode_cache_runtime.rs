#![cfg(feature = "anthropic-gateway")]
//! The Go catalog cache through the real gateway: fresh, stale, refresh, refusal.
#![allow(dead_code, unexpected_cfgs)] // Shared gateway fixtures; this file uses JSON replies only.
#[path = "common/anthropic.rs"]
mod anthropic;
#[path = "common/skeleton.rs"]
mod skeleton;
#[path = "common/openai.rs"]
mod upstream;
use oxc_gateway::{
    catalog::Access,
    gateway::Gateway,
    opencode::{
        cache::{Cache, State, FRESH, STALE},
        models,
    },
    runtime::{Options, Runtime},
    secrets,
};
use serde_json::{json, Value};
use skeleton::Fixture;
use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime},
};
use upstream::{MockOpenAi, Reply};

const CREDENTIAL: &str = "synthetic-provider-credential";
const TIMEOUT: Duration = Duration::from_secs(5);

fn discovery(base: &str) -> Value {
    json!({"data":[{"id":"synthetic-flash","canonical_id":"synthetic/canonical","limit":{"context":32768,"output":512},
        "api":{"chat-completions":{"base_url":format!("{base}/inference/go/openai/v1")}}}]})
}

fn go_fixture() -> Fixture {
    let fixture = Fixture::for_endpoint("https://example.invalid/v1");
    fs::write(&fixture.key_path, json!({"opencode": CREDENTIAL}).to_string()).unwrap();
    fixture
}

impl Fixture {
    /// The cache sits beside the ledger, in the same private agent directory.
    fn cache(&self) -> PathBuf {
        self.ledger_dir.parent().unwrap().join("go-catalog-cache.json")
    }

    fn age(&self, origin: &str, base: &str, seconds: u64) {
        let key = secrets::read(&self.key_path, "opencode").unwrap();
        let known = models(&discovery(origin), base).unwrap();
        Cache::new(self.ledger_dir.parent().unwrap())
            .store(
                base,
                &key,
                &known,
                SystemTime::now() - Duration::from_secs(seconds),
            )
            .unwrap();
    }
}

fn go_gateway(fixture: &Fixture, access: Access) -> Gateway {
    let runtime = Runtime::new(
        [],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access,
            ..Options::default()
        },
    )
    .unwrap();
    Gateway::with_read_timeout(runtime.with_catalog_cache(), Some(TIMEOUT)).unwrap()
}

#[test]
fn a_fresh_entry_serves_startup_and_a_stale_one_is_used_then_refreshed() {
    let fixture = go_fixture();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::json(discovery(base)),
            Reply::error(500, "server_error", "synthetic upstream failure"),
            Reply::json(discovery(base)),
        ]
    });
    let origin = format!("http://{}", mock.address());
    let base = format!("{origin}/inference/go");
    let mut gateway = go_gateway(&fixture, Access::Enabled);
    assert_eq!(gateway.discover_opencode(&base, TIMEOUT).unwrap(), State::Fresh);
    assert_eq!(mock.requests().len(), 1, "the first start discovers live");
    assert!(fixture.cache().is_file(), "live discovery publishes an entry");
    assert_eq!(gateway.discover_opencode(&base, TIMEOUT).unwrap(), State::Fresh);
    assert_eq!(
        mock.requests().len(),
        1,
        "a fresh entry is consumed without a network call"
    );
    fixture.age(&origin, &base, FRESH.as_secs() + 60);
    assert_eq!(gateway.discover_opencode(&base, TIMEOUT).unwrap(), State::Stale);
    assert_eq!(
        mock.requests().len(),
        1,
        "a stale entry is used now, never after the network"
    );
    gateway.refresh_opencode(&base, TIMEOUT).unwrap();
    assert_eq!(mock.requests().len(), 2, "the refresh is the only later call");
    assert_eq!(
        gateway.discover_opencode(&base, TIMEOUT).unwrap(),
        State::Fresh,
        "a successful refresh publishes a fresh entry"
    );
    assert_eq!(mock.requests().len(), 2);
    fixture.age(&origin, &base, FRESH.as_secs() + 60);
    assert!(
        gateway.refresh_opencode(&base, TIMEOUT).is_err(),
        "a failed refresh is an error to its own caller"
    );
    assert_eq!(mock.requests().len(), 3);
    assert_eq!(
        gateway.discover_opencode(&base, TIMEOUT).unwrap(),
        State::Stale,
        "a failed refresh leaves the previous entry intact and non-fatal"
    );
    assert_eq!(mock.requests().len(), 3);
    fixture.age(&origin, &base, STALE.as_secs() + 60);
    assert_eq!(
        gateway.discover_opencode(&base, TIMEOUT).unwrap(),
        State::Fresh,
        "an expired entry falls back to live discovery"
    );
    assert_eq!(mock.requests().len(), 4);
    mock.assert_healthy();
}

#[test]
fn offline_and_cloud_off_refuse_before_the_cache_or_any_network_call() {
    let fixture = go_fixture();
    let mock = MockOpenAi::start_with(|base| vec![Reply::json(discovery(base))]);
    let origin = format!("http://{}", mock.address());
    let base = format!("{origin}/inference/go");
    let mut gateway = go_gateway(&fixture, Access::Enabled);
    assert_eq!(gateway.discover_opencode(&base, TIMEOUT).unwrap(), State::Fresh);
    assert_eq!(mock.requests().len(), 1);
    for access in [Access::Offline, Access::Disabled] {
        let mut gateway = go_gateway(&fixture, access);
        assert!(
            gateway.discover_opencode(&base, TIMEOUT).is_err(),
            "a cached entry never authorizes a refused route: {access:?}"
        );
        assert!(
            gateway.refresh_opencode(&base, TIMEOUT).is_err(),
            "a refused route never refreshes: {access:?}"
        );
    }
    assert_eq!(mock.requests().len(), 1, "no refusal reaches the network");
    mock.assert_healthy();
}

#[test]
fn go_plan_usage_reads_only_typed_windows_at_the_discovered_base() {
    use oxa_proto::CancelToken;
    use oxc_gateway::opencode::{PlanUsage, PlanWindow, PlanWindowStatus};

    let fixture = go_fixture();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::json(json!({"usage": {
                "rolling": {"status": "ok", "percent": 0, "resetsAt": "2026-11-18T11:00:00.001+01:00"},
                "weekly": {"status": "rate-limited", "percent": 100, "resetsAt": CREDENTIAL},
                "monthly": {"status": CREDENTIAL, "percent": 72.5}
            }})),
            Reply::json(json!({"usage": {
                "rolling": {"percent": 101},
                "renewed": {"fraction": 0.4}
            }})),
            Reply::error(429, CREDENTIAL, CREDENTIAL).buffer_header_refusal(),
        ]
    });
    let base = format!("http://{}/inference/go", mock.address());
    let mut enabled = go_gateway(&fixture, Access::Enabled);
    enabled.discover_opencode(&base, TIMEOUT).unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancel = CancelToken::new();
    let read = |gateway: &Gateway| {
        runtime
            .block_on(gateway.opencode_plan_observation(TIMEOUT, Duration::ZERO, &cancel))
            .map(|observation| observation.map(|observation| observation.usage))
    };
    for access in [Access::Offline, Access::Disabled] {
        assert_eq!(read(&go_gateway(&fixture, access)).unwrap(), None);
    }
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(
        read(&enabled).unwrap(),
        Some(PlanUsage {
            rolling: Some(PlanWindow {
                status: Some(PlanWindowStatus::Ok),
                used_percent: Some(0.0),
                resets_at: Some(1794996001),
            }),
            weekly: Some(PlanWindow {
                status: Some(PlanWindowStatus::RateLimited),
                used_percent: Some(100.0),
                resets_at: None,
            }),
            monthly: Some(PlanWindow {
                status: None,
                used_percent: Some(72.5),
                resets_at: None,
            }),
        })
    );
    assert_eq!(read(&enabled).unwrap(), None);
    let error = read(&enabled).unwrap_err();
    assert_eq!(error.refusal().code, "cloud_rate_limited");
    assert!(!format!("{error:?} {error}").contains(CREDENTIAL));
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests[1..] {
        assert_eq!(request.method, "GET");
        assert_eq!(request.target, "/inference/go/v1/usage");
        assert!(request
            .header("authorization")
            .is_some_and(|value| value == format!("Bearer {CREDENTIAL}")));
    }
    mock.assert_healthy();
}

#[test]
fn go_plan_usage_cache_respects_age_and_current_credential() {
    use oxa_proto::CancelToken;

    const ROTATED: &str = "synthetic-rotated-provider-credential";
    let fixture = go_fixture();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::json(json!({"usage":{"weekly":{"percent":3}}})),
            Reply::json(json!({"usage":{"weekly":{"percent":8}}})),
            Reply::json(json!({"usage":{"weekly":{"percent":14}}})),
        ]
    });
    let base = format!("http://{}/inference/go", mock.address());
    let mut gateway = go_gateway(&fixture, Access::Enabled);
    gateway.discover_opencode(&base, TIMEOUT).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancel = CancelToken::new();
    let read = |max_age| {
        runtime
            .block_on(gateway.opencode_plan_observation(TIMEOUT, max_age, &cancel))
            .unwrap()
            .unwrap()
    };
    let max_age = Duration::from_secs(60);
    let first = read(max_age);
    assert_eq!(read(max_age), first, "a reuse retains the acquisition time");
    assert_eq!(
        runtime
            .block_on(gateway.opencode_plan_usage(TIMEOUT, &cancel))
            .unwrap()
            .as_ref(),
        Some(&first.usage),
    );
    assert_eq!(mock.requests().len(), 2, "a cache hit sends no new Usage request");

    let refreshed = read(Duration::ZERO);
    assert_eq!(refreshed.usage.weekly.unwrap().used_percent, Some(8.0));
    fs::write(&fixture.key_path, json!({"opencode":ROTATED}).to_string()).unwrap();
    let rotated = read(max_age);
    assert_eq!(rotated.usage.weekly.unwrap().used_percent, Some(14.0));
    fs::write(&fixture.key_path, "{}").unwrap();
    assert_eq!(
        runtime
            .block_on(gateway.opencode_plan_observation(TIMEOUT, max_age, &cancel))
            .unwrap(),
        None,
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[3]
        .header("authorization")
        .is_some_and(|value| value == format!("Bearer {ROTATED}")));
    mock.assert_healthy();
}
