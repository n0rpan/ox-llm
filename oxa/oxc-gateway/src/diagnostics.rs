//! Opt-in, ephemeral facts for the trusted CLI. Never part of an error or ledger.
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    LocalCredentialRead,
    DiscoveryHttp,
    ChatHttp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub stage: Stage,
    pub status: Option<u16>,
    pub code: Option<&'static str>,
    /// Fixed order: Retry-After, limit, remaining, reset. Numeric values only.
    pub rate: [Option<u32>; 4],
}

/// One native CLI operation at a time; retries clear the preceding operation.
/// API callers do not install this sink. No serialization or user supplied text.
#[derive(Clone, Default)]
pub struct Diagnostics(Arc<Mutex<Option<Snapshot>>>);
impl Diagnostics {
    pub fn take(&self) -> Option<Snapshot> {
        self.0.lock().ok()?.take()
    }
    pub(crate) fn clear(&self) {
        let _ = self.take();
    }
    pub(crate) fn local(&self) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(Snapshot {
                stage: Stage::LocalCredentialRead,
                status: None,
                code: None,
                rate: [None; 4],
            });
        }
    }
    pub(crate) fn http(&self, stage: Stage, status: u16, headers: &HeaderMap) {
        let names = [
            "retry-after",
            "x-ratelimit-limit",
            "x-ratelimit-remaining",
            "x-ratelimit-reset",
        ];
        let rate = names.map(|name| {
            let mut values = headers.get_all(name).iter();
            let text = values.next()?.to_str().ok()?;
            if values.next().is_some()
                || text.is_empty()
                || text.len() > 10
                || !text.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            text.parse::<u32>().ok()
        });
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(Snapshot {
                stage,
                status: Some(status),
                code: None,
                rate,
            });
        }
    }
    pub(crate) fn error_code(&self, body: &Value) {
        let known = |value: &Value| match value.as_str()? {
            "RateLimit" => Some("RateLimit"),
            "rate_limit_error" => Some("rate_limit_error"),
            "subscription_limit_reached" => Some("subscription_limit_reached"),
            "inference_failed" => Some("inference_failed"),
            "invalid_request_error" => Some("invalid_request_error"),
            "authentication_error" => Some("authentication_error"),
            _ => None,
        };
        let code = known(&body["error"]["type"]).or_else(|| known(&body["error"]["code"]));
        if let Ok(mut slot) = self.0.lock() {
            if let Some(snapshot) = slot.as_mut() {
                snapshot.code = code;
            }
        }
    }
}
