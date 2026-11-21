//! #1446: one feed keeps a worker's GitHub packet current.
//!
//! The frozen `Task.prompt` stays the task's identity (native finish and `matches_issue_packet`
//! compare it). Updates are follow-ups, never a rewrite of the task: at every worker launch
//! (first run, continue after an interruption, repair round, `--rerun`) the runner asks the feed
//! for the current packet and adds only the lines that are new. The worker stays
//! GitHub-isolated; the runner fetches. GitHub text is context, never an approval.
use crate::{append_capped, Result, Task, MAX_COMMENT_BYTES};
use serde::{Deserialize, Serialize};

/// A source of the current packet of a task.
pub trait PacketFeed: Send + Sync {
    /// The packet as GitHub has it now, or `None` when there is nothing to fetch.
    fn fresh(&self, task: &Task) -> Result<Option<String>>;
    /// Hand a follow-up to a running worker session (its inbox, as `oxa send` does).
    fn deliver(&self, session: &str, text: &str) -> Result<()>;
}

/// What the runner last showed the worker: stored on the record, absent in old checkpoints.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PacketState {
    /// The last packet text delivered (starts as the frozen prompt).
    pub seen: String,
    pub at_ms: u64,
    /// Follow-ups delivered so far.
    pub updates: u32,
}

/// The lines of `fresh` that `seen` does not have, capped; `None` when nothing is new.
pub fn delta(seen: &str, fresh: &str) -> Option<String> {
    let known: std::collections::HashSet<&str> = seen.lines().map(str::trim_end).collect();
    let mut out = String::new();
    let mut any = false;
    for line in fresh.lines().map(str::trim_end) {
        if line.trim().is_empty() || known.contains(line) {
            continue;
        }
        any = true;
        if append_capped(&mut out, &[line, "\n"], MAX_COMMENT_BYTES) {
            break;
        }
    }
    any.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_comment_yields_only_the_new_lines() {
        let seen = "Implement issue #1: t\n\nbody\n\nby alice:\nfirst\n";
        let fresh = "Implement issue #1: t\n\nbody\n\nby alice:\nfirst\n\nby bob:\nplease also X\n";
        assert_eq!(delta(seen, fresh).as_deref(), Some("by bob:\nplease also X\n"));
    }

    #[test]
    fn an_unchanged_packet_yields_nothing() {
        assert_eq!(delta("a\nb\n", "a\nb\n"), None);
        assert_eq!(delta("a\nb\n", "b\n\na\n"), None);
    }

    #[test]
    fn the_update_is_capped() {
        let fresh = format!("new\n{}", "x".repeat(MAX_COMMENT_BYTES * 2));
        let text = delta("old\n", &fresh).unwrap();
        assert!(text.len() <= MAX_COMMENT_BYTES, "{}", text.len());
    }
}
