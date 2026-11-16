//! Optional, typed progress observation. No message copies, serialization or worker thread.
use crate::{RunOutcome, SessionState};
use oxa_proto::{TurnKind, Usage};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy)]
pub enum Progress<'a> {
    Session { file: Option<&'a Path> },
    RunStart,
    RequestStart { kind: TurnKind, remote: bool },
    RequestEnd { usage: Option<Usage>, remote: bool },
    Advanced,
    ToolStart(&'a str),
    ToolEnd,
    Settled(RunOutcome),
    Finished(RunOutcome),
}

/// Observers must be cheap and must not panic or call back into the session. Front ends own
/// any I/O errors and surface them when the driver returns. Stream observations carry no text.
pub trait ProgressObserver: std::fmt::Debug + Send + Sync {
    fn notify(&self, session_id: &str, progress: Progress<'_>);
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
        observer.notify(&st.sref.id, progress);
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
