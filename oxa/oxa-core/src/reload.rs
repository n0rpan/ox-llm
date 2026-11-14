//! R2 (wave H lane `local`, FIT F4): the engine reloads the model mid-session.
//!
//! After an idle or pressure unload the next request pays a model load (37-43 s on the
//! target machine) or meets a restarting engine. What oxa does:
//! * **Retry before the first byte, same request.** `chat_with_retry` already retries
//!   `engine_down`, 5xx and `engine_busy` before any data byte with the same `Idempotency-Key`
//!   (the key is fixed once per logical request and reused by every attempt; tested).
//! * **One "model loading" notice** per logical request, at the first retry whose error says
//!   the model is loading or the engine is (re)starting ([`loading_notice`]).
//! * **Fresh capabilities and limits.** A request that recovered from such a retry sets
//!   `Gate::reread_info` (F4, in `retry.rs`). A reload the client could not see (the
//!   controller held the request while it loaded the model; the engine restarted between
//!   turns) shows in the reply: the engine held nothing of the session (`cached == 0` where
//!   the transcript expected a cache, on a request that is not the process's first).
//!   [`after_reply`] then sets `reread_info` too and says so once, so the loop re-reads
//!   `/_local/status` / `/props` before the next request and the hints (`prefill_only`,
//!   snapshots, hint clears) and limits are not stale.
use crate::SessionState;
use oxa_proto::{codes, problem_text, AgentEvent, EventSink, Level, OxaError};
use std::sync::atomic::Ordering;

/// Whether a retried error means the engine is loading the model or (re)starting.
pub(crate) fn is_loading(e: &OxaError) -> bool {
    let m = e.message.to_ascii_lowercase();
    match e.code {
        codes::ENGINE_DOWN => true,
        codes::ENGINE_HTTP => m.contains("503") || m.contains("loading") || m.contains("start"),
        codes::ENGINE_BUSY => m.contains("loading") || m.contains("start"),
        _ => false,
    }
}

/// The notice text (R6 shape).
pub fn loading_text(e: &OxaError) -> String {
    problem_text(
        "Model loading",
        &format!("the engine is loading the model or restarting ({})", e.message),
        "none needed: oxa sends the same request again when the engine is ready; Esc cancels",
    )
}

/// The notice after a reload the client only saw in the reply (R6 shape).
pub const RELOADED_TEXT: &str = "Engine reloaded: it held nothing of this session, so the whole conversation was prefilled again (the model was unloaded and loaded, or the engine restarted)\nFix: none needed: oxa read the engine's capabilities and limits again";

/// At a retry of `chat_with_retry`: one notice per logical request when `e` is a loading error.
pub(crate) fn loading_notice(e: &OxaError, noticed: &mut bool, sink: &mut dyn EventSink) {
    if *noticed || !is_loading(e) {
        return;
    }
    *noticed = true;
    sink.emit(&AgentEvent::Notice {
        level: Level::Info,
        text: loading_text(e),
    });
}

/// After a reply of the loop, before its re-read check: a reply with an empty engine cache
/// where one was expected (not the first request of the process) marks a reload.
pub(crate) fn after_reply(
    st: &SessionState,
    first: bool,
    expected: u32,
    cached: u32,
    sink: &mut dyn EventSink,
) {
    if first || expected == 0 || cached != 0 {
        return;
    }
    st.shared.gate.reread_info.store(true, Ordering::SeqCst);
    sink.emit(&AgentEvent::Notice {
        level: Level::Info,
        text: RELOADED_TEXT.to_string(),
    });
}
