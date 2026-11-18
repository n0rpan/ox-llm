#![cfg(all(feature = "anthropic-client", feature = "ledger"))]
//! Bounded stale-aware Go catalog cache: no network, no key, no transcript.
#![allow(dead_code, unexpected_cfgs)] // Shared synthetic gateway fixture.
#[path = "common/anthropic.rs"]
mod anthropic;
#[path = "common/skeleton.rs"]
mod skeleton;

use oxc_gateway::opencode::{
    cache::{Cache, State, FRESH, MAX_BYTES, STALE},
    models, Model,
};
use oxc_gateway::secrets::{self, Key};
use serde_json::{json, Map, Value};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const BASE: &str = "https://example.invalid/inference/go";
const CREDENTIAL: &str = "synthetic-provider-credential";

fn discovery() -> Value {
    json!({"data":[{"id":"synthetic-flash","canonical_id":"synthetic/canonical","limit":{"context":32768,"output":512},
        "api":{"chat-completions":{"base_url":"https://example.invalid/inference/go/openai/v1","compatibility":{"reasoning_field":"reasoning_content","note":CREDENTIAL}}},
        "cost":{"input":0.1},"capabilities":{"tools":true},"modalities":{"input":["text"],"output":["text"]}},
        {"id":"synthetic-messages","canonical_id":"synthetic/messages","limit":{"context":32768,"output":512},
        "api":{"messages":{"base_url":"https://example.invalid/inference/go/anthropic/v1"}}}]})
}

impl skeleton::Fixture {
    fn cache(&self) -> Cache {
        Cache::new(self.ledger_dir.parent().unwrap())
    }
    fn key(&self, value: &str) -> Key {
        fs::write(&self.key_path, json!({"opencode": value}).to_string()).unwrap();
        secrets::read(&self.key_path, "opencode").unwrap()
    }
    fn file(&self) -> PathBuf {
        self.ledger_dir.parent().unwrap().join("go-catalog-cache.json")
    }
    fn write(&self, bytes: &[u8]) {
        fs::write(self.file(), bytes).unwrap();
    }
}

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// The fields the cache is allowed to carry, so a round trip is compared on
/// exactly those and not on the unvalidated API hints it deliberately drops.
fn shape(model: &Model) -> Value {
    let api: Map<String, Value> = model
        .api
        .iter()
        .map(|(shape, api)| (shape.clone(), json!(api.base_url)))
        .collect();
    json!({"id":model.id,"canonical_id":model.canonical_id,"context":model.context_window,
        "output":model.max_output,"api":api,"cost":model.cost,
        "capabilities":model.capabilities,"modalities":model.modalities})
}

#[test]
fn fresh_and_stale_entries_are_consumed_and_expired_ones_are_misses() {
    let dir = skeleton::Fixture::for_endpoint(BASE);
    let cache = dir.cache();
    let key = dir.key(CREDENTIAL);
    let models = models(&discovery(), BASE).unwrap();
    cache.store(BASE, &key, &models, at(1_000)).unwrap();
    let (state, loaded) = cache.load(BASE, &key, at(1_000 + FRESH.as_secs() - 1)).unwrap();
    assert_eq!(state, State::Fresh);
    assert_eq!(loaded.len(), models.len());
    for (loaded, original) in loaded.iter().zip(&models) {
        assert_eq!(shape(loaded), shape(original));
    }
    let (state, loaded) = cache.load(BASE, &key, at(1_000 + FRESH.as_secs())).unwrap();
    assert_eq!(state, State::Stale, "the fresh window ends at FRESH");
    assert_eq!(shape(&loaded[0]), shape(&models[0]));
    let (state, _) = cache.load(BASE, &key, at(1_000 + STALE.as_secs() - 1)).unwrap();
    assert_eq!(state, State::Stale);
    assert!(
        cache.load(BASE, &key, at(1_000 + STALE.as_secs())).is_none(),
        "an entry at the stale limit is expired, so startup discovers live"
    );
    assert!(
        cache.load(BASE, &key, at(999)).is_none(),
        "a future-dated entry is a miss, never a fresh one"
    );
}

#[test]
fn missing_malformed_oversized_and_foreign_base_entries_are_misses() {
    let dir = skeleton::Fixture::for_endpoint(BASE);
    let cache = dir.cache();
    let key = dir.key(CREDENTIAL);
    assert!(cache.load(BASE, &key, at(1_000)).is_none(), "missing");
    dir.write(b"{not json");
    assert!(cache.load(BASE, &key, at(1_000)).is_none(), "malformed");
    dir.write(&vec![b'x'; MAX_BYTES as usize + 1]);
    assert!(cache.load(BASE, &key, at(1_000)).is_none(), "oversized");
    let models = models(&discovery(), BASE).unwrap();
    cache.store(BASE, &key, &models, at(1_000)).unwrap();
    assert!(
        cache
            .load("https://example.invalid/other", &key, at(1_000))
            .is_none(),
        "an entry from another trusted base is never reused"
    );
    assert!(cache.load(BASE, &key, at(1_000)).is_some());
    let rotated = dir.key("synthetic-rotated-credential");
    assert!(cache.load(BASE, &rotated, at(1_000)).is_none());
}

#[test]
fn a_tampered_entry_cannot_widen_routing_or_limits() {
    let dir = skeleton::Fixture::for_endpoint(BASE);
    let cache = dir.cache();
    let key = dir.key(CREDENTIAL);
    let models = models(&discovery(), BASE).unwrap();
    cache.store(BASE, &key, &models, at(1_000)).unwrap();
    let good: Value = serde_json::from_slice(&fs::read(dir.file()).unwrap()).unwrap();
    for (case, mutate) in [
        (
            "another origin",
            Box::new(|value: &mut Value| {
                value["data"][0]["api"]["chat-completions"]["base_url"] =
                    json!("https://other.invalid/inference/go/openai/v1")
            }) as Box<dyn Fn(&mut Value)>,
        ),
        (
            "outside the trusted path",
            Box::new(|value: &mut Value| {
                value["data"][0]["api"]["chat-completions"]["base_url"] =
                    json!("https://example.invalid/outside/v1")
            }),
        ),
        (
            "plausible price change",
            Box::new(|value: &mut Value| value["data"][0]["cost"]["input"] = json!(0.001)),
        ),
        (
            "output above context",
            Box::new(|value: &mut Value| value["data"][0]["limit"]["output"] = json!(32769)),
        ),
        (
            "invalid id",
            Box::new(|value: &mut Value| value["data"][0]["id"] = json!("invalid id")),
        ),
        (
            "duplicate id",
            Box::new(|value: &mut Value| {
                let first = value["data"][0].clone();
                value["data"].as_array_mut().unwrap().push(first);
            }),
        ),
    ] {
        let mut value = good.clone();
        mutate(&mut value);
        dir.write(&serde_json::to_vec(&value).unwrap());
        assert!(
            cache.load(BASE, &key, at(1_000)).is_none(),
            "a tampered entry ({case}) is a miss, never a partial catalog"
        );
    }
    dir.write(&serde_json::to_vec(&good).unwrap());
    assert!(cache.load(BASE, &key, at(1_000)).is_some());
}

#[test]
fn replacement_is_atomic_and_leaves_no_temporary_file() {
    let dir = skeleton::Fixture::for_endpoint(BASE);
    let cache = dir.cache();
    let key = dir.key(CREDENTIAL);
    let first = models(&discovery(), BASE).unwrap();
    cache.store(BASE, &key, &first, at(1_000)).unwrap();
    let second = models(
        &json!({"data":[{"id":"synthetic-other","canonical_id":"synthetic/other","limit":{"context":4096,"output":64},
            "api":{"chat-completions":{"base_url":"https://example.invalid/inference/go/openai/v1"}}}]}),
        BASE,
    )
    .unwrap();
    cache.store(BASE, &key, &second, at(2_000)).unwrap();
    let (state, loaded) = cache.load(BASE, &key, at(2_000)).unwrap();
    assert_eq!(state, State::Fresh);
    assert_eq!(loaded.len(), 1, "last rename wins");
    assert_eq!(loaded[0].id, "synthetic-other");
    let names: Vec<_> = fs::read_dir(dir.ledger_dir.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 3);
    assert!(names.iter().any(|name| name == "ledger"));
    assert!(names.iter().any(|name| name == "go-catalog-cache.json"));
    assert!(names.iter().any(|name| name == "credentials.json"));
    let mode = fs::metadata(dir.file()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "the entry is private");
}

#[test]
fn only_validated_metadata_is_persisted() {
    let dir = skeleton::Fixture::for_endpoint(BASE);
    let cache = dir.cache();
    let key = dir.key(CREDENTIAL);
    let models = models(&discovery(), BASE).unwrap();
    cache.store(BASE, &key, &models, at(1_000)).unwrap();
    let raw = fs::read_to_string(dir.file()).unwrap();
    assert!(
        !raw.contains(CREDENTIAL),
        "a credential echoed into an API hint is never persisted"
    );
    for forbidden in [
        "authorization",
        "bearer",
        "x-opencode-session",
        "reasoning_content",
    ] {
        assert!(
            !raw.to_lowercase().contains(forbidden),
            "the cache never carries {forbidden}"
        );
    }
    let value: Value = serde_json::from_str(&raw).unwrap();
    let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["auth", "base", "data", "fetched_at"]);
    for item in value["data"].as_array().unwrap() {
        let mut keys: Vec<_> = item.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "api",
                "canonical_id",
                "capabilities",
                "cost",
                "id",
                "limit",
                "modalities"
            ]
        );
        for api in item["api"].as_object().unwrap().values() {
            assert_eq!(
                api.as_object().unwrap().keys().collect::<Vec<_>>(),
                ["base_url"],
                "unknown API hints are not persisted"
            );
        }
    }
}
