//! OpenCode adapter for the shared catalog cache.
use super::{models_diagnostic, Model};
use crate::secrets::Key;
use serde_json::{json, Map, Value};
use std::{io, path::Path, time::SystemTime};

pub use crate::catalog_cache::{State, FRESH, MAX_BYTES, STALE};

#[derive(Clone)]
pub struct Cache(crate::catalog_cache::Cache);

impl Cache {
    pub fn new(directory: &Path) -> Self {
        Self(crate::catalog_cache::Cache::new(
            directory,
            "go-catalog-cache.json",
        ))
    }

    pub fn load(&self, base: &str, key: &Key, now: SystemTime) -> Option<(State, Vec<Model>)> {
        let (state, value) = self.0.load(key, now)?;
        if value["base"].as_str() != Some(base) {
            return None;
        }
        Some((state, models_diagnostic(&value, base).ok()?))
    }

    pub fn store(&self, base: &str, key: &Key, models: &[Model], now: SystemTime) -> io::Result<()> {
        self.0.store(
            key,
            json!({"base": base, "data": models.iter().map(entry).collect::<Vec<_>>()}),
            now,
        )
    }
}

/// Only the fields the strict parser accepted are persisted. Unknown API hints
/// are dropped: nothing reads them, and they are the one unvalidated blob.
fn entry(model: &Model) -> Value {
    let api: Map<String, Value> = model
        .api
        .iter()
        .map(|(shape, api)| (shape.clone(), json!({"base_url": api.base_url})))
        .collect();
    json!({
        "id": model.id,
        "canonical_id": model.canonical_id,
        "limit": {"context": model.context_window, "output": model.max_output},
        "api": api,
        "cost": model.cost,
        "capabilities": model.capabilities,
        "modalities": model.modalities,
    })
}
