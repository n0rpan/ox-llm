//! B15: golden request hashes.
use crate::{cache_bytes, Recorded};
use oxa_proto::{codes, OxaError};
use sha2::{Digest, Sha256};
use std::path::Path;

fn flag(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One hash line per request: lowercase hex SHA-256 over `cache_bytes(system, tools_json,
/// messages)` + `\0` + `max_tokens` + `\0` + `thinking` + `\0` + `kind` + `\0` + `background` +
/// `\0` + `snapshot_keep` (session ids and idempotency keys are excluded: they are random).
pub fn request_hashes(reqs: &[Recorded]) -> Vec<String> {
    reqs.iter()
        .map(|r| {
            let mut v = cache_bytes(&r.system, &r.tools_json, &r.messages);
            v.push(0);
            v.extend_from_slice(r.max_tokens.to_string().as_bytes());
            v.push(0);
            v.extend_from_slice(flag(r.thinking).as_bytes());
            v.push(0);
            v.extend_from_slice(r.kind.as_bytes());
            v.push(0);
            v.extend_from_slice(flag(r.background).as_bytes());
            v.push(0);
            v.extend_from_slice(flag(r.snapshot_keep).as_bytes());
            hex(&Sha256::digest(&v))
        })
        .collect()
}

/// Compare with the golden file (one hash per line). `OXA_UPDATE_GOLDEN=1` semantics are the
/// caller's: `update = true` rewrites the file and returns `Ok`. A mismatch is `invalid` with
/// the first differing line number and both hashes.
pub fn compare_golden(path: &Path, hashes: &[String], update: bool) -> Result<(), OxaError> {
    if update {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| OxaError::io("create golden directory", &e))?;
        }
        let mut text = String::new();
        for h in hashes {
            text.push_str(h);
            text.push('\n');
        }
        return std::fs::write(path, text).map_err(|e| OxaError::io("write golden file", &e));
    }
    let text = std::fs::read_to_string(path).map_err(|e| {
        OxaError::io("read golden file", &e).with_hint("Run with OXA_UPDATE_GOLDEN=1 to write it.")
    })?;
    let expected: Vec<&str> = text.lines().collect();
    for i in 0..expected.len().max(hashes.len()) {
        let want = expected.get(i).copied().unwrap_or("<no line>");
        let got = hashes.get(i).map(String::as_str).unwrap_or("<no request>");
        if want != got {
            return Err(OxaError::new(
                codes::INVALID,
                format!("golden mismatch at line {}: expected {want}, got {got}", i + 1),
            )
            .with_hint("If the change is intended, rerun with OXA_UPDATE_GOLDEN=1."));
        }
    }
    Ok(())
}
