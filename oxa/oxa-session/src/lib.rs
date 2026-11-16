//! W0-FROZEN (architect-owned).
//!
//! # oxa-session
//! Session files (pi's JSONL v3 tree), the transcript fold and tree operations.
//!
//! ## Contracts
//! * **W1.** The wire is a pure fold of the active branch: [`Transcript::project`]. For every
//!   entry that is not a rebasing body the fold only appends. `Transcript::rebase` is the one
//!   place the wire may shrink or change, and it records why (`oxa_proto::rebase_why`).
//! * **W2.** An assistant `Msg` entry stores the exact wire fields as streamed; `encode_entry`
//!   and `decode_entry` round-trip them byte for byte (`load(save(x)) == x`).
//! * **W3.** The prefix of an epoch is an entry (`EntryBody::Prefix`, a pi `message` entry with role `system`) and is
//!   replayed from the file on resume, never rebuilt.
//! * **File safety.** Files are created `0600` inside `0700` directories, opened with
//!   `O_APPEND | O_NOFOLLOW`; a symlinked session path is refused (`unsafe_path`). A torn last
//!   line (no trailing `\n` or invalid JSON) is dropped on load, never repaired in place.
//! * **Layout.** `<sessions_dir>/--<cwd with '/' -> '-'>--/<timestamp>_<id>.jsonl` (pi's scheme),
//!   first line = the session header.

mod clock;
mod durable;
mod lock;
mod meta;
mod ops;
mod store;
mod transcript;
mod tree;

pub use clock::{local_time, LocalTime};
pub use lock::{lock_path_of, SessionLock, LOCKED};
pub use meta::{meta_of, search_sessions, sort_sessions, SessionSort};
pub use ops::{clone_session, fork_file, fork_session, set_label, set_name};
pub use store::{
    count_files, decode_entry, decode_header, encode_entry, encode_header, new_entry_id, purge_session_file,
    remove_all_spill, remove_spill_dir, remove_spill_dir_unless_forked, session_dir_name,
    session_files_under, FsStore, MemStore,
};
pub use transcript::{bash_text, RebaseLog, Transcript};
pub use tree::Tree;

use oxa_proto::{Entry, OxaError};
use std::path::{Path, PathBuf};

/// A session file (or an in-memory session of [`MemStore`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionRef {
    pub path: PathBuf,
    /// The session id (uuid v4 text unless given by `--session-id`).
    pub id: String,
}

/// The first line of a session file (pi's `SessionHeader`, `type: "session"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHeader {
    /// Always `oxa_proto::names::SESSION_VERSION` when written.
    pub version: u32,
    pub id: String,
    pub ts_ms: u64,
    pub cwd: PathBuf,
    /// Path of the session this one was forked or cloned from (pi's `parentSession`).
    pub parent_session: Option<String>,
}

/// What the session picker and `list` show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    pub sref: SessionRef,
    pub cwd: PathBuf,
    /// Display name from the last `SessionInfo` entry.
    pub name: Option<String>,
    pub created_ms: u64,
    pub modified_ms: u64,
    /// Number of user + assistant messages.
    pub message_count: u32,
    /// First user message, single line, for the picker.
    pub first_message: String,
    /// All user + assistant text, for search.
    pub all_text: String,
    pub parent_session: Option<String>,
}

/// Session persistence. [`FsStore`] is the file store; [`MemStore`] backs `--no-session`.
pub trait SessionStore: Send + Sync {
    /// Create a session for `cwd` and write its header. `id` = `--session-id` (must be new);
    /// `parent` = the file this session derives from.
    fn create(&self, cwd: &Path, id: Option<&str>, parent: Option<&Path>) -> Result<SessionRef, OxaError>;
    /// Append one entry as one line with a single `write` (O_APPEND, 0600 in 0700, O_NOFOLLOW).
    fn append(&self, s: &SessionRef, e: &Entry) -> Result<(), OxaError>;
    /// All entries in file order. A torn last line is dropped.
    fn load(&self, s: &SessionRef) -> Result<Vec<Entry>, OxaError>;
    fn header(&self, s: &SessionRef) -> Result<SessionHeader, OxaError>;
    /// Sessions of `cwd`, or of every directory when `None`; unreadable files are skipped.
    fn list(&self, cwd: Option<&Path>) -> Vec<SessionMeta>;
    /// Resolve a path, a full id or a unique id prefix (`--session`, `--fork`).
    fn resolve(&self, id_or_path: &str) -> Result<SessionRef, OxaError>;
    /// Delete through the `trash` command when available, else unlink (pi's behaviour).
    fn delete(&self, s: &SessionRef) -> Result<(), OxaError>;
    /// R1: make the appends so far durable (`fdatasync` of the session file). Called once at
    /// the end of each turn, not per entry: a power loss costs at most the turn in progress.
    /// The default (in-memory stores) has nothing to sync.
    fn sync(&self, _s: &SessionRef) -> Result<(), OxaError> {
        Ok(())
    }
    /// Become the one writer of `s`: refuse (code `LOCKED`) when another process has it open.
    /// The lock is held until another session is acquired or the store is dropped. In-memory
    /// stores have nothing to lock.
    ///
    /// Two-phase so that a failed switch never unlocks the session still in use: `acquire`
    /// only STAGES the new lock (the current one is untouched); [`SessionStore::commit_lock`]
    /// makes it the held one and drops the old, [`SessionStore::abort_lock`] drops it.
    fn acquire(&self, _s: &SessionRef) -> Result<(), OxaError> {
        Ok(())
    }
    /// The staged lock of [`SessionStore::acquire`] becomes the held one; the old is released.
    fn commit_lock(&self) {}
    /// The staged lock is released; the held one is untouched.
    fn abort_lock(&self) {}
    /// [`SessionStore::delete`], telling how the file went (pi's picker says "Session moved
    /// to trash" or "Session deleted"). Default: `delete`, reported as [`Removed::Deleted`].
    fn remove(&self, s: &SessionRef) -> Result<Removed, OxaError> {
        self.delete(s).map(|()| Removed::Deleted)
    }
}

/// How [`SessionStore::remove`] removed a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removed {
    /// The `trash` command took it (or the file was gone after it ran).
    Trashed,
    /// Unlinked (no `trash`, or it failed).
    Deleted,
}
