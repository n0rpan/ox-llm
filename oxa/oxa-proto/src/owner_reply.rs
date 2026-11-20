//! #1552: the owner's own typed `yes` as approval for a publication.
//!
//! The authority test is the origin of the message. The only caller of [`typed`] is the
//! interactive front end, from the line the owner typed into its input; text the model read from
//! a tool result, a file, the web or another session never reaches it. A publication that
//! shows its exact text and waits calls [`await_reply`] with the digest of that text; a `yes`
//! the owner types afterwards approves exactly that text once ([`take_approved`]). Any other
//! line the owner types withdraws the pending text and any approval of it.
use std::sync::Mutex;

#[derive(Default)]
struct State {
    pending: Option<String>,
    approved: Option<String>,
}

static STATE: Mutex<State> = Mutex::new(State {
    pending: None,
    approved: None,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|p| p.into_inner())
}

/// The exact text identified by `digest` was shown to the owner, who has not answered yet.
pub fn await_reply(digest: &str) {
    let mut s = state();
    s.pending = Some(digest.to_string());
    s.approved = None;
}

/// A line the OWNER typed into the interactive input (call from there only).
pub fn typed(line: &str) {
    let mut s = state();
    if line.trim().eq_ignore_ascii_case("yes") {
        if let Some(pending) = s.pending.take() {
            s.approved = Some(pending);
        }
    } else {
        s.pending = None;
        s.approved = None;
    }
}

/// `true` once, when the owner typed `yes` for exactly this text.
pub fn take_approved(digest: &str) -> bool {
    let mut s = state();
    if s.approved.as_deref() == Some(digest) {
        s.approved = None;
        return true;
    }
    false
}

/// Withdraw a pending text and its approval (the owner denied it).
pub fn clear() {
    let mut s = state();
    s.pending = None;
    s.approved = None;
}
