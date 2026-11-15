//! Bounded authenticated calls to the engine-owned cache and numeric preload receipts.
use crate::engine::{self, Backend};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::io;

pub async fn request(
    backend: &Backend,
    method: reqwest::Method,
    path: &str,
    payload: Option<&Value>,
    statuses: &[u16],
    limit: usize,
    timeout: f64,
) -> io::Result<Value> {
    let error = || io::Error::other("state cache engine call failed");
    let client = engine::client(timeout);
    let mut request = client
        .request(method, format!("{}{path}", backend.url))
        .bearer_auth(&backend.key)
        .header("content-type", "application/json");
    if let Some(payload) = payload {
        request = request.json(payload);
    }
    let response = request.send().await.map_err(|_| error())?;
    if !statuses.contains(&response.status().as_u16()) {
        return Err(error());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| error())?;
        if bytes.len() + chunk.len() > limit {
            return Err(error());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(error)
}
pub fn preload_view(value: &Value) -> (&'static str, Value, Value) {
    let ms = value["ms"]
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 0.0 && *v < 1e9)
        .map(|v| json!((v * 10.0).round_ties_even() / 10.0))
        .unwrap_or(Value::Null);
    match value["status"].as_str() {
        Some("started" | "running") => ("running", Value::Null, Value::Null),
        Some("done" | "used") => ("done", json!("disk"), ms),
        Some("live") => ("done", json!("live"), Value::Null),
        Some("busy") => ("cancelled", Value::Null, Value::Null),
        Some("cancelled" | "failed") => ("cancelled", Value::Null, ms),
        Some("none" | "idle") => ("idle", Value::Null, Value::Null),
        _ => ("unavailable", Value::Null, Value::Null),
    }
}
