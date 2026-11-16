//! Defensive classification of the private subscription interface, not API quotas.
use reqwest::header::HeaderMap;
use std::time::Duration;

/// Subscription refusals retain only typed metadata, never provider messages.
/// Separate from the API refusal enum so existing API callers stay unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    UsageLimitReached {
        plan_type: Option<String>,
        /// Absolute Unix seconds, not a relative duration.
        resets_at: Option<u64>,
        retry_after: Option<Duration>,
    },
    UsageNotIncluded,
}

/// Recognize only documented HTTP 429 / `error.type` pairs from #627.
/// Bodies over 64 KiB are rejected before JSON parsing. Missing or malformed
/// optional metadata is discarded independently; unknown error shapes are `None`.
/// No message matching, requests, clock reads, or retry policy are involved.
pub fn classify_chatgpt_refusal(status: u16, body: &[u8], headers: &HeaderMap) -> Option<Refusal> {
    if status != 429 || body.len() > 64 * 1024 {
        return None;
    }
    let body: serde_json::Value = serde_json::from_slice(body).ok()?;
    let error = body.get("error")?.as_object()?;
    match error.get("type")?.as_str()? {
        "usage_limit_reached" => Some(Refusal::UsageLimitReached {
            plan_type: error.get("plan_type").and_then(|v| v.as_str()).map(str::to_owned),
            resets_at: error.get("resets_at").and_then(|v| v.as_u64()),
            retry_after: super::retry_after(headers),
        }),
        "usage_not_included" => Some(Refusal::UsageNotIncluded),
        _ => None,
    }
}
