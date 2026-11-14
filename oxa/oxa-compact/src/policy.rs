//! B1: when to compact. Owner: task B1. Signatures below are frozen; bodies are yours.
// adapted from pi packages/coding-agent/src/core/compaction/compaction.ts (shouldCompact)
/// Limits in tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactPolicy {
    /// Compact in idle time after a run that ends above this.
    pub soft: u32,
    /// Compact mid-run above this: `n_ctx - max_tokens - reserve_tokens`.
    pub hard: u32,
    /// Tokens of recent messages kept verbatim after the summary (default 4000).
    pub keep_tokens: u32,
    /// `max_tokens` of the summary request.
    pub summary_max: u32,
    /// `compaction.enabled`; when false only the overflow path (`MidRun`) remains.
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    No,
    AfterReply,
    MidRun,
}

/// Build the policy from settings values and the engine's limits. `soft_setting = None` means
/// 75% of `hard`. `hard` = `n_ctx - max_output - reserve_tokens` (saturating, at least 1024);
/// `soft` is clamped below `hard`; `summary_max` = `min(max_output, reserve_tokens * 4 / 5)`.
pub fn policy_from(
    n_ctx: u32,
    max_output: u32,
    reserve_tokens: u32,
    keep_recent_tokens: u32,
    soft_setting: Option<u32>,
    enabled: bool,
) -> CompactPolicy {
    /// Smallest hard limit: a context window this small cannot hold a useful session.
    const MIN_HARD: u32 = 1024;
    let hard = n_ctx
        .saturating_sub(max_output)
        .saturating_sub(reserve_tokens)
        .max(MIN_HARD);
    // 75% of hard by default; the setting may not reach hard itself (and never 0).
    let soft_default = (u64::from(hard) * 3 / 4) as u32;
    let soft = soft_setting.unwrap_or(soft_default).min(hard - 1).max(1);
    let summary_max = max_output.min((u64::from(reserve_tokens) * 4 / 5) as u32);
    CompactPolicy {
        soft,
        hard,
        keep_tokens: keep_recent_tokens,
        summary_max,
        enabled,
    }
}

/// `context` = `Transcript::context()`. `context > hard` -> `MidRun` (always, also when
/// disabled); else `run_ended && enabled && context > soft` -> `AfterReply`; else `No`.
pub fn decide(context: u32, run_ended: bool, p: &CompactPolicy) -> When {
    if context > p.hard {
        When::MidRun
    } else if run_ended && p.enabled && context > p.soft {
        When::AfterReply
    } else {
        When::No
    }
}
