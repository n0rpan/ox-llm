//! V020-OPTIMIZE #13 (`oxa.prefill.paste`): when the interactive loop asks the worker to
//! prefill the editor's draft. Pure (the clock, the draft and the worker's generations are
//! passed in), so the debounce and the resend rules are tested without a terminal.
use oxa_proto::Part;
use std::hash::{Hash, Hasher};

/// The key of a draft worth a prefill: a hash of what Enter would send, or `None` when it is
/// not prefilled (a slash command, a `!` line, or surely under the size floor; the core checks
/// the exact size). `text` is the submitted text (`submitted_text`). Cheap: text parts are
/// hashed as they are, an image by its path, digest and size (never formatted).
pub(crate) fn draft_key(text: &str, parts: &[Part]) -> Option<u64> {
    // A cheap lower bound (image parts add placeholder lines).
    if text.len() + 80 * parts.len() < oxa_core::Session::DRAFT_PREFILL_MIN_BYTES
        || text.starts_with('/')
        || text.starts_with('!')
    {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    parts.len().hash(&mut h);
    for part in parts {
        match part {
            Part::Text(t) => (0u8, t).hash(&mut h),
            Part::Image {
                path,
                sha256,
                w,
                h: ht,
                ..
            } => (1u8, path, sha256, w, ht).hash(&mut h),
        }
    }
    Some(h.finish())
}

/// The debounce and once-per-draft state of the idle tick.
#[derive(Debug, Default)]
pub(crate) struct DraftTrigger {
    /// The draft seen last and since when (ms) it is unchanged.
    seen: Option<(u64, u64)>,
    /// The draft last sent for prefill and the generation it was taken at.
    sent: Option<(u64, u64)>,
}

impl DraftTrigger {
    /// One idle tick at `now` (ms). `draft` is [`draft_key`] of the editor, or `None` while
    /// no prefill may start (a run, a preload or a draft prefill is active, a dialog or a
    /// selector is open, or the draft is not eligible). `gen` is the current draft generation,
    /// `done` the generation of the last completed prefill. Returns `true` when the draft is
    /// to be sent now (taken at `gen`).
    ///
    /// A draft is sent once it has been unchanged for `Session::DRAFT_PREFILL_QUIET`, and once
    /// per draft and wire, unless its prefill did not complete: when the generation moved
    /// since it was sent (a key, even one that left the draft unchanged) and that prefill is
    /// not the last completed one, it was skipped or cancelled and the draft is sent again.
    pub(crate) fn tick(&mut self, now: u64, draft: Option<u64>, gen: u64, done: Option<u64>) -> bool {
        let Some(hash) = draft else {
            self.seen = None;
            return false;
        };
        if let Some((sent, at)) = self.sent {
            if sent == hash && gen != at && done != Some(at) {
                self.sent = None;
            }
        }
        match self.seen {
            Some((seen, since)) if seen == hash => {
                let quiet = oxa_core::Session::DRAFT_PREFILL_QUIET.as_millis() as u64;
                if now.saturating_sub(since) >= quiet && self.sent.map(|s| s.0) != Some(hash) {
                    self.sent = Some((hash, gen));
                    return true;
                }
                false
            }
            _ => {
                self.seen = Some((hash, now));
                false
            }
        }
    }

    /// The wire may have changed (the worker answered a command): the same draft is a new
    /// prefill.
    pub(crate) fn wire_changed(&mut self) {
        self.sent = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: u64 = oxa_core::Session::DRAFT_PREFILL_QUIET.as_millis() as u64;

    fn big() -> String {
        "x".repeat(oxa_core::Session::DRAFT_PREFILL_MIN_BYTES)
    }

    #[test]
    fn slash_bang_and_small_drafts_are_not_prefilled() {
        assert!(draft_key(&big(), &[]).is_some());
        assert_eq!(draft_key(&format!("/compact {}", big()), &[]), None);
        assert_eq!(draft_key(&format!("!cat {}", big()), &[]), None);
        assert_eq!(draft_key("short", &[]), None);
    }

    #[test]
    fn key_follows_text_and_parts() {
        let image = |sha: u8| Part::Image {
            path: "/tmp/a.png".into(),
            mime: "image/png".into(),
            sha256: [sha; 32],
            w: 10,
            h: 20,
        };
        let a = draft_key(&big(), &[image(1)]);
        assert_eq!(a, draft_key(&big(), &[image(1)]));
        assert_ne!(a, draft_key(&big(), &[image(2)]));
        assert_ne!(a, draft_key(&big(), &[image(1), Part::Text("t".into())]));
        assert_ne!(draft_key(&big(), &[]), draft_key(&format!("{}y", big()), &[]));
    }

    /// Debounce: sent only after the draft was unchanged for the quiet time; an edit restarts
    /// it; sent once.
    #[test]
    fn debounce_then_once() {
        let mut t = DraftTrigger::default();
        assert!(!t.tick(1000, Some(1), 0, None), "first sight starts the clock");
        assert!(!t.tick(1000 + QUIET - 1, Some(1), 0, None));
        // An edit at that moment restarts the wait.
        assert!(!t.tick(1000 + QUIET - 1, Some(2), 1, None));
        assert!(!t.tick(1000 + QUIET, Some(2), 1, None));
        assert!(
            t.tick(1000 + 2 * QUIET - 1, Some(2), 1, None),
            "quiet long enough"
        );
        assert!(!t.tick(1000 + 3 * QUIET, Some(2), 1, None), "once per draft");
        // The prefill completed; a non-editing key moves the generation: not sent again.
        assert!(!t.tick(1000 + 4 * QUIET, Some(2), 2, Some(1)));
        // The wire changed: the same draft is a new prefill.
        t.wire_changed();
        assert!(t.tick(1000 + 5 * QUIET, Some(2), 2, Some(1)));
    }

    /// Nothing while a run (or a dialog, a preload, a prefill) is active, and the quiet time
    /// starts again after it.
    #[test]
    fn none_while_blocked() {
        let mut t = DraftTrigger::default();
        assert!(!t.tick(0, Some(7), 0, None));
        for now in [QUIET, 2 * QUIET, 10 * QUIET] {
            assert!(!t.tick(now, None, 0, None));
        }
        assert!(
            !t.tick(10 * QUIET + 1, Some(7), 0, None),
            "the clock starts again"
        );
        assert!(t.tick(11 * QUIET + 1, Some(7), 0, None));
    }

    /// A key that leaves the draft unchanged cancels (or makes stale) the prefill in flight:
    /// the same draft is sent again once quiet, at the new generation.
    #[test]
    fn resend_after_a_cancel_by_a_non_editing_key() {
        let mut t = DraftTrigger::default();
        assert!(!t.tick(0, Some(3), 5, None));
        assert!(t.tick(QUIET, Some(3), 5, None), "sent at gen 5");
        // An arrow key: gen 6, the prefill taken at 5 was cancelled (never completed).
        assert!(t.tick(QUIET + 10, Some(3), 6, None), "sent again at once");
        // That one completed: no more.
        assert!(!t.tick(QUIET + 20, Some(3), 7, Some(6)));
        // A stale completion of an older one does not count for the newer.
        let mut u = DraftTrigger::default();
        assert!(!u.tick(0, Some(3), 1, None));
        assert!(u.tick(QUIET, Some(3), 1, None));
        assert!(u.tick(QUIET + 1, Some(3), 2, Some(0)));
    }
}
