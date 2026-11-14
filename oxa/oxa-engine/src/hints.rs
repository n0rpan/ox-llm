//! A2: hint headers H1-H6. Owner: task A2. Signatures below are frozen; bodies are yours.
use oxa_proto::Hints;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The hint headers for one request, in this fixed order, each only when applicable:
/// `X-Oxe-Session` and `X-Session-Id` (H1, both, when `session` is non-empty),
/// `Idempotency-Key` (H2, when non-empty), `X-Oxe-Priority: background` (H3, when
/// `background`), `X-Oxe-Snapshot: keep` (H4, when `snapshot_keep`), `X-Oxe-Turn-Kind` (H5,
/// always, `TurnKind::as_str`). H6 is `max_tokens: 0` in the body. Pure; golden-tested.
pub fn build_headers(h: &Hints) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::with_capacity(6);
    if !h.session.is_empty() {
        out.push(("X-Oxe-Session", h.session.to_string()));
        out.push(("X-Session-Id", h.session.to_string()));
    }
    if !h.idem_key.is_empty() {
        out.push(("Idempotency-Key", h.idem_key.to_string()));
    }
    if h.background {
        out.push(("X-Oxe-Priority", "background".to_string()));
    }
    if h.snapshot_keep {
        out.push(("X-Oxe-Snapshot", "keep".to_string()));
    }
    out.push(("X-Oxe-Turn-Kind", h.kind.as_str().to_string()));
    out
}

/// A fresh idempotency key: 32 lowercase hex characters from the OS random source. One key per
/// logical request; a retry reuses it.
///
/// The OS source is `/dev/urandom`. Where it cannot be read, the key is mixed from the clock,
/// the process id and the per-process hash seeds of the standard library; that path is only a
/// fallback and keeps the key unique, not secret.
pub fn new_idem_key() -> String {
    let mut bytes = [0u8; 16];
    let from_os = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok();
    if !from_os {
        fallback_bytes(&mut bytes);
    }
    let mut key = String::with_capacity(32);
    for b in bytes {
        key.push(char::from(b"0123456789abcdef"[usize::from(b >> 4)]));
        key.push(char::from(b"0123456789abcdef"[usize::from(b & 0x0f)]));
    }
    key
}

fn fallback_bytes(bytes: &mut [u8; 16]) {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    for (i, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut h = RandomState::new().build_hasher();
        h.write_u64(nanos);
        h.write_u64(u64::from(std::process::id()));
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_usize(i);
        let word = h.finish().to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
}
