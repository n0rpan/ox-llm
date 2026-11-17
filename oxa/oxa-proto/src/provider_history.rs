//! Private provider replay material, separate from the visible conversation.
use serde_json::{json, Value};
use std::{fmt, io, sync::Arc};

pub const MAX_PROVIDER_HISTORY_ITEMS: usize = 512;
pub const MAX_PROVIDER_HISTORY_BYTES: usize = 4 * 1024 * 1024;

/// Bounded native output items for one completed assistant message, in wire order.
///
/// The gateway authenticates `scope` (provider, route, model and stable account)
/// and `prefix` (frozen prefix and visible history) before replay. Construction
/// validates storage bounds, not those HMACs. Creators must supply provider output
/// only: never credentials, request headers or refresh tokens.
///
/// This is session-private data: never include it in visible wire JSON, hook
/// payloads, token estimates, status, logs or ordinary engine requests. An absent
/// carrier costs one pointer; clones share immutable storage without copying items.
#[derive(Clone, PartialEq, Eq)]
pub struct ProviderHistory(Arc<History>);

#[derive(PartialEq, Eq)]
struct History {
    scope: [u8; 32],
    prefix: [u8; 32],
    items: Arc<[Value]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderHistoryError {
    Malformed,
    UnsupportedVersion,
    TooManyItems,
    TooLarge,
}
impl fmt::Display for ProviderHistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "Malformed provider history.",
            Self::UnsupportedVersion => "Unsupported provider history version.",
            Self::TooManyItems => "Too many provider history items.",
            Self::TooLarge => "Provider history exceeds the byte limit.",
        })
    }
}
impl std::error::Error for ProviderHistoryError {}

impl fmt::Debug for ProviderHistory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderHistory([redacted])")
    }
}

impl ProviderHistory {
    pub fn new(scope: [u8; 32], prefix: [u8; 32], items: Vec<Value>) -> Result<Self, ProviderHistoryError> {
        Self::from_shared_items(scope, prefix, items.into())
    }
    /// Share already-owned native output without copying a potentially large
    /// private replay array. The same bounds apply to both constructors.
    pub fn from_shared_items(
        scope: [u8; 32],
        prefix: [u8; 32],
        items: Arc<[Value]>,
    ) -> Result<Self, ProviderHistoryError> {
        validate_items(&items)?;
        Ok(Self(Arc::new(History { scope, prefix, items })))
    }
    pub fn scope(&self) -> &[u8; 32] {
        &self.0.scope
    }
    pub fn prefix(&self) -> &[u8; 32] {
        &self.0.prefix
    }
    pub fn items(&self) -> &[Value] {
        &self.0.items
    }

    /// Explicit private session encoding; deliberately no general Serialize impl.
    pub fn to_session_value(&self) -> Value {
        json!({"v":1,"scope":hex(self.scope()),"prefix":hex(self.prefix()),"items":self.items()})
    }

    /// Parse a present carrier. Absence is handled by the session codec; a present
    /// null, malformed, oversized or unknown-version carrier must not be discarded.
    pub fn from_session_value(value: &Value) -> Result<Self, ProviderHistoryError> {
        use ProviderHistoryError::{Malformed, UnsupportedVersion};
        let map = value.as_object().ok_or(Malformed)?;
        let version = map.get("v").and_then(Value::as_u64).ok_or(Malformed)?;
        if version != 1 {
            return Err(UnsupportedVersion);
        }
        if map.len() != 4 {
            return Err(Malformed);
        }
        let scope = unhex(map.get("scope").and_then(Value::as_str).ok_or(Malformed)?)?;
        let prefix = unhex(map.get("prefix").and_then(Value::as_str).ok_or(Malformed)?)?;
        let items = map.get("items").and_then(Value::as_array).ok_or(Malformed)?;
        // Check before cloning an untrusted persisted array.
        validate_items(items)?;
        Ok(Self(Arc::new(History {
            scope,
            prefix,
            items: items.clone().into(),
        })))
    }
}

fn validate_items(items: &[Value]) -> Result<(), ProviderHistoryError> {
    if items.len() > MAX_PROVIDER_HISTORY_ITEMS {
        return Err(ProviderHistoryError::TooManyItems);
    }
    if items.iter().any(|item| !item.is_object()) {
        return Err(ProviderHistoryError::Malformed);
    }
    // Count actual encoded UTF-8 bytes (including escapes and array delimiters)
    // without allocating another potentially unbounded serialization buffer.
    struct Bounded(usize);
    impl io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(io::ErrorKind::FileTooLarge.into());
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Bounded(MAX_PROVIDER_HISTORY_BYTES), items)
        .map_err(|_| ProviderHistoryError::TooLarge)
}
fn hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| {
            [
                DIGITS[(b >> 4) as usize] as char,
                DIGITS[(b & 15) as usize] as char,
            ]
        })
        .collect()
}
fn unhex(text: &str) -> Result<[u8; 32], ProviderHistoryError> {
    fn digit(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        }
    }
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return Err(ProviderHistoryError::Malformed);
    }
    let mut out = [0; 32];
    for (pair, byte) in bytes.as_chunks::<2>().0.iter().zip(&mut out) {
        *byte = digit(pair[0])
            .zip(digit(pair[1]))
            .map(|(hi, lo)| hi * 16 + lo)
            .ok_or(ProviderHistoryError::Malformed)?;
    }
    Ok(out)
}
