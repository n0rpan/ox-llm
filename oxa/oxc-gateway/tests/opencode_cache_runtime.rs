#![cfg(feature = "anthropic-gateway")]
//! The Go catalog cache through the real gateway: fresh, stale, refresh, refusal.
#![allow(dead_code, unexpected_cfgs)] // Shared loopback mock; this file uses JSON replies only.
#[path = "common/openai.rs"]
mod upstream;
use oxc_gateway::{
    billing::SubscriptionLimits,
    catalog::{Access, Model, Provider},
    gateway::Gateway,
    opencode::cache::{State, FRESH, STALE},
    runtime::{Billing, Options, Runtime},
};
use serde_json::{json, Value};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use upstream::{MockOpenAi, Reply};

const CREDENTIAL: &str = "synthetic-provider-credential";
const MODEL: &str = "opencode/synthetic-flash";
const TIMEOUT: Duration = Duration::from_secs(5);

fn discovery(base: &str) -> Value {
    json!({"data":[{"id":"synthetic-flash","canonical_id":"synthetic/canonical","limit":{"context":32768,"output":512},
        "api":{"chat-completions":{"base_url":format!("{base}/inference/go/openai/v1")}}}]})
}

/// A private directory of this test only; never the real home.
struct Fixture {
    root: PathBuf,
    key_path: PathBuf,
    ledger_dir: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "oxc-go-runtime-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .unwrap();
        let key_path = root.join("credentials.json");
        let mut key = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)
            .unwrap();
        write!(key, "{}", json!({"opencode": CREDENTIAL})).unwrap();
        let ledger_dir = root.join("ledger");
        DirBuilder::new().mode(0o700).create(&ledger_dir).unwrap();
        Self {
            root,
            key_path,
            ledger_dir,
        }
    }
    /// The cache sits beside the ledger, in the same private agent directory.
    fn cache(&self) -> PathBuf {
        self.root.join("go-catalog-cache.json")
    }
    /// Age the published entry without touching its validated contents.
    fn age(&self, seconds: u64) {
        let mut value: Value = serde_json::from_slice(&fs::read(self.cache()).unwrap()).unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        value["fetched_at"] = json!(now - seconds);
        fs::write(self.cache(), serde_json::to_vec(&value).unwrap()).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn go_gateway(fixture: &Fixture, base: &str, access: Access) -> Gateway {
    let model = Model::new(
        MODEL,
        Provider::OpenCode,
        "synthetic-flash",
        &format!("{base}/inference/go/openai/v1"),
        32768,
        512,
    )
    .unwrap();
    let runtime = Runtime::new(
        [(model, Some(Billing::Subscription))],
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        Options {
            access,
            subscription_limits: Some(SubscriptionLimits::new(20, 2_000_000).unwrap()),
            ..Options::default()
        },
    )
    .unwrap();
    Gateway::with_read_timeout(runtime, Some(TIMEOUT)).unwrap()
}

#[test]
fn a_fresh_entry_serves_startup_and_a_stale_one_is_used_then_refreshed() {
    let fixture = Fixture::new();
    let mock = MockOpenAi::start_with(|base| {
        vec![
            Reply::json(discovery(base)),
            Reply::json(discovery(base)),
            Reply::error(500, "server_error", "synthetic upstream failure"),
            Reply::json(discovery(base)),
        ]
    });
    let base = format!("{}/inference/go", mock.base_url());
    let mut gateway = go_gateway(&fixture, &base, Access::Enabled);
    assert_eq!(gateway.discover_opencode(&base, TIMEOUT).unwrap(), State::Fresh);
    assert_eq!(mock.requests().len(), 1, "the first start discovers live");
    assert!(fixture.cache().is_file(), "live discovery publishes an entry");
    assert_eq!(gateway.discover_opencode(&base, TIMEOUT).unwrap(), State::Fresh);
    assert_eq!(
        mock.requests().len(),
        1,
        "a fresh entry is consumed without a network call"
    );
    fixture.age(FRESH.as_secs() + 60);
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
    fixture.age(FRESH.as_secs() + 60);
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
    fixture.age(STALE.as_secs() + 60);
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
    let fixture = Fixture::new();
    let mock = MockOpenAi::start_with(|base| vec![Reply::json(discovery(base))]);
    let base = format!("{}/inference/go", mock.base_url());
    let mut gateway = go_gateway(&fixture, &base, Access::Enabled);
    assert_eq!(gateway.discover_opencode(&base, TIMEOUT).unwrap(), State::Fresh);
    assert_eq!(mock.requests().len(), 1);
    for access in [Access::Offline, Access::Disabled] {
        let mut gateway = go_gateway(&fixture, &base, access);
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
