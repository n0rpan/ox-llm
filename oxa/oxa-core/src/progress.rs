//! Optional, typed progress observation. No message copies, serialization or worker thread.
use crate::{RunOutcome, SessionState};
use oxa_proto::{TurnKind, Usage};
use oxa_session::SessionRef;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy)]
pub enum Progress<'a> {
    Session {
        file: Option<&'a Path>,
    },
    RunStart,
    /// The frozen prefix (system text + tool list) the next request sends: its SHA-256
    /// (`FrozenPrefix::sha256`) and its byte count. Sent right before `RequestStart`. Digest
    /// and length only, never content: `--usage-log` records them to show the prefix is stable.
    Prefix {
        sha256: [u8; 32],
        bytes: usize,
    },
    RequestStart {
        kind: TurnKind,
        remote: bool,
    },
    RequestEnd {
        usage: Option<Usage>,
        remote: bool,
    },
    Advanced,
    ToolStart(&'a str),
    ToolEnd,
    /// The distinct file paths this run's successful `edit`/`write` calls named, in first-use
    /// order, reported once when the run records its end (#2297). Tool-observed only: a shell
    /// rewrite names no file here. Acceptance evidence, not a verdict; nothing is refused.
    FilesChanged {
        paths: &'a [String],
    },
    /// A native goal control advanced the revision and the session store confirmed the sync.
    /// The operation may still need a prefix refresh. This is a wake to read the bound owner.
    GoalControlCommitted {
        revision: u64,
    },
    /// The saved goal's status changed; `none` once it is cleared. A pause carries its stored reason.
    GoalStatus {
        status: &'static str,
        reason: Option<&'a str>,
    },
    /// A limit or SIGTERM turned this run into its wrap-up: `time`, `tokens` or `signal`.
    WrapUp {
        limit: &'static str,
    },
    /// An operation returned, including an empty queue or a command without an agent turn.
    /// This observation does not imply an AgentSettled event or hook was emitted.
    Settled(RunOutcome),
    Closing,
    /// The front end has its aggregate result and has finished shutdown.
    Finished(RunOutcome),
}

/// Observers must be cheap and must not panic or call back into the session. Front ends own
/// any I/O errors and surface them when the driver returns. Stream observations carry no text.
/// The full session reference distinguishes equal custom IDs in different directories.
pub trait ProgressObserver: std::fmt::Debug + Send + Sync {
    fn notify(&self, session: &SessionRef, progress: Progress<'_>);
}

pub(crate) fn observer(st: &SessionState) -> Option<Arc<dyn ProgressObserver>> {
    st.shared
        .gate
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

pub(crate) fn notify(st: &SessionState, progress: Progress<'_>) {
    if let Some(observer) = observer(st) {
        observer.notify(&st.sref, progress);
    }
}

pub(crate) fn session(st: &SessionState) {
    notify(
        st,
        Progress::Session {
            file: session_file(st),
        },
    );
}

pub(crate) fn session_file(st: &SessionState) -> Option<&Path> {
    let path = &st.sref.path;
    (!path.as_os_str().is_empty() && !path.to_string_lossy().starts_with("mem:")).then_some(path)
}

/// The `Prefix` progress of a request: digest and size of the frozen prefix it sends.
pub(crate) fn prefix_progress(prefix: &oxa_proto::FrozenPrefix) -> Progress<'static> {
    Progress::Prefix {
        sha256: prefix.sha256,
        bytes: prefix.hash_input().len(),
    }
}
