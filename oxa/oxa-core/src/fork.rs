//! #1844 PR 2: a fork child's head.
//!
//! On a local model the `subagent` tool forks by default instead of starting an isolated child
//! (`subagent.rs`): the child's first request continues the parent's prompt, so the engine
//! keeps the parent's state instead of evicting it and prefilling it again afterwards (45-100 s
//! at 12K tokens on a laptop).
//!
//! What a fork child needs is the parent's head, byte for byte:
//! * the parent's frozen prefix ([`oxa_proto::FrozenPrefix`]: the system text and the tool list)
//!   and
//! * the parent's wire.
//!
//! Both are exactly what a session file holds, so the seed is the parent's active branch in the
//! session-file line format ([`oxa_session::encode_entry`]): the same codec a `--resume` reads,
//! and therefore the same prefix and the same messages. The parent asks its session for it
//! (`HostAction::ForkSeed`), writes it to an owner-only file under the session's spill directory
//! and sets [`ENV_CHILD_FORK`] to that path. The child reads the file once, before its first
//! prompt, and installs it as its own head ([`crate::Session::adopt_fork`]). The file is deleted
//! when the child is gone; it holds the parent's conversation, so it is `0600` in a `0700`
//! directory and nothing else reads it.
use crate::SessionState;
use oxa_proto::{codes, Entry, OxaError, Value};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Env: the path of the fork seed a parent wrote for this child. A child without it is an
/// isolated child (or a remote one: cloud children never fork).
pub const ENV_CHILD_FORK: &str = "OXA_CHILD_FORK";

/// A seed larger than this is refused unread (a parent wire of 100K tokens is a few MiB).
const MAX_SEED_BYTES: u64 = 64 << 20;

/// The parent's head for a fork child: its active branch in the session-file line format (one
/// encoded entry per line). The prefix and the wire are both replayed from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkSeed {
    pub entries: String,
}

impl ForkSeed {
    /// The answer of [`HostAction::ForkSeed`](oxa_proto::HostAction::ForkSeed).
    pub fn from_host(v: &Value) -> Result<ForkSeed, OxaError> {
        let entries = v
            .get("entries")
            .and_then(Value::as_str)
            .ok_or_else(|| OxaError::new(codes::INTERNAL, "the fork seed has no entries"))?;
        Ok(ForkSeed {
            entries: entries.to_string(),
        })
    }

    /// Write the seed to `path`: an owner-only file in an owner-only directory, never following
    /// a symlink and never replacing an existing file.
    pub fn write(&self, path: &Path) -> Result<(), OxaError> {
        use std::os::unix::fs::DirBuilderExt as _;
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| OxaError::io(&format!("create {}", dir.display()), &e))?;
        }
        let body = json!({ "entries": self.entries }).to_string();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| OxaError::io(&format!("write {}", path.display()), &e))?;
        use std::io::Write as _;
        file.write_all(body.as_bytes())
            .map_err(|e| OxaError::io(&format!("write {}", path.display()), &e))
    }

    /// Read a seed written by [`ForkSeed::write`].
    pub fn read(path: &Path) -> Result<ForkSeed, OxaError> {
        let file =
            std::fs::File::open(path).map_err(|e| OxaError::io(&format!("read {}", path.display()), &e))?;
        let mut bytes = Vec::new();
        file.take(MAX_SEED_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|e| OxaError::io(&format!("read {}", path.display()), &e))?;
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|e| OxaError::new(codes::INVALID, format!("the fork seed is not JSON: {e}")))?;
        ForkSeed::from_host(&v)
    }

    /// The branch the seed holds, decoded. A malformed line is an error: a child never starts a
    /// prompt on a partial head.
    pub fn branch(&self) -> Result<Vec<Entry>, OxaError> {
        self.entries
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| oxa_session::decode_entry(l.as_bytes()))
            .collect()
    }
}

/// The answer of [`HostAction::ForkSeed`](oxa_proto::HostAction::ForkSeed): the session's head,
/// or an error before its first epoch (nothing is frozen yet, so there is no shared prefix).
pub(crate) fn seed_of(st: &SessionState) -> Result<Value, OxaError> {
    if st.prefix.sections.is_empty() {
        return Err(OxaError::new(
            codes::INVALID,
            "the session has no frozen prefix yet, so a child cannot fork it",
        ));
    }
    let branch = st.tree.branch(st.tree.leaf());
    let mut buf = Vec::new();
    for entry in &branch {
        oxa_session::encode_entry(entry, &mut buf);
        buf.push(b'\n');
    }
    let entries = String::from_utf8(buf)
        .map_err(|e| OxaError::new(codes::INTERNAL, format!("the session branch is not UTF-8: {e}")))?;
    Ok(json!({ "entries": entries }))
}

/// The prefix and the wire of a seed, for [`crate::Session::adopt_fork`]: the last `Prefix`
/// entry's frozen prefix and the messages replayed from the branch.
pub(crate) fn replay(branch: &[Entry]) -> (Option<oxa_proto::FrozenPrefix>, Vec<oxa_proto::WireMsg>) {
    let transcript = oxa_session::Transcript::project(branch);
    (transcript.prefix().cloned(), transcript.wire().to_vec())
}
