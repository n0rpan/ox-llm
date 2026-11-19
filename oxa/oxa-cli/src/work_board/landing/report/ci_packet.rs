//! The failed-job log tail that a runner packet carries to a worker that cannot fetch job logs.
#![cfg_attr(not(test), allow(dead_code))]
use crate::report_cmd::{scrub, ScrubCtx};

/// The newest bytes of a failed job log that a packet carries.
pub(super) const MAX_LOG_TAIL_BYTES: usize = 8 * 1024;

/// Redact the whole log before cutting it: a cut first could leave half of a credential that no
/// rule matches. Returns the newest redacted bytes and whether older bytes were dropped.
pub(super) fn log_tail(log: &str, ctx: &ScrubCtx) -> (String, bool) {
    let clean = scrub(log, ctx);
    if clean.len() <= MAX_LOG_TAIL_BYTES {
        return (clean, false);
    }
    let mut start = clean.len() - MAX_LOG_TAIL_BYTES;
    while !clean.is_char_boundary(start) {
        start += 1;
    }
    (clean[start..].to_string(), true)
}

/// One CI data packet: the run, the redacted failed job names and the capped log tail.
pub(super) fn ci_packet(run: u64, failed_jobs: &[String], log: &str, ctx: &ScrubCtx) -> String {
    let jobs = if failed_jobs.is_empty() {
        "none listed".to_owned()
    } else {
        scrub(&failed_jobs.join(", "), ctx)
    };
    let (tail, cut) = log_tail(log, ctx);
    let mut packet = format!("CI run {run} failed. Failed jobs: {jobs}.\n\nFailed job log tail, as data:\n");
    if cut {
        packet.push_str("--- older log lines omitted ---\n");
    }
    packet.push_str(&tail);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ScrubCtx {
        ScrubCtx {
            home: None,
            user: None,
            host: None,
        }
    }

    #[test]
    fn ci_packet_carries_the_failed_log_tail() {
        let packet = ci_packet(
            12,
            &["test (ubuntu)".to_owned()],
            "build ok\nerror: boom in the parser\n",
            &ctx(),
        );
        assert!(
            packet.starts_with("CI run 12 failed. Failed jobs: test (ubuntu)."),
            "{packet}"
        );
        assert!(packet.contains("error: boom in the parser"), "{packet}");
        assert!(!packet.contains("older log lines omitted"), "{packet}");
    }

    #[test]
    fn log_tail_is_capped_and_redacted() {
        let log = format!(
            "{}password=hunter2-fixture\nAuthorization: Bearer fixture-token-123\nFAILED final-line\n",
            "filler line\n".repeat(2000)
        );
        let (tail, cut) = log_tail(&log, &ctx());
        assert!(cut);
        assert!(tail.len() <= MAX_LOG_TAIL_BYTES, "{}", tail.len());
        assert!(tail.ends_with("FAILED final-line\n"), "{tail}");
        assert!(tail.contains("password=<secret>"), "{tail}");
        assert!(!tail.contains("hunter2-fixture"), "{tail}");
        assert!(!tail.contains("fixture-token-123"), "{tail}");
    }

    #[test]
    fn short_logs_are_kept_whole_and_marked_as_not_cut() {
        let (tail, cut) = log_tail("one\ntwo\n", &ctx());
        assert!(!cut);
        assert_eq!(tail, "one\ntwo\n");
    }
}
