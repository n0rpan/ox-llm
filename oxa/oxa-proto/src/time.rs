//! Shared wall-clock timestamps.
use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix timestamp in milliseconds, saturating at the bounds of `u64`.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
