//! Per-epoch tuning for small-context models (V020 #2, OXA-LOCAL-FIRST T4, T8, F5, R5).
//!
//! The core decides how the built-in tools behave ([`ToolTune`]) and adapts the local profile to
//! a small context. The only input about the model is its engine limit `n_ctx`
//! (`SessionState::info.n_ctx`, the engine's published context window), never a model name.
//! Under profile `pi` with no new setting every function here returns what pi returns:
//! [`tune_for`] is `ToolTune::default()`, [`output_cap_for`] the setting, and [`keep_tokens`]
//! the policy's own value.
//!
//! What is true now: the `local` flag of the tune changes only at a prefix epoch, because it
//! must match the tool declarations of the frozen prefix (`Stats::profile`). The page size, the
//! diet and the bash timeout follow the context window and the settings at the next budget
//! refresh ([`crate::session::refresh_budget`], run on `/reload`, a model switch and around a
//! run while the budget is provisional) and at an epoch; [`refresh`] recomputes them, and the
//! same-bytes branch of an epoch calls it too. No new prefix epoch is needed for them.
use crate::SessionState;
use oxa_config::Settings;
use oxa_proto::{Profile, ToolTune};

/// A context window below this is "small" (T4, R5). The same bound `subagent::MIN_CONTEXT`
/// uses for the subagent tool.
pub(crate) const SMALL_CONTEXT: u32 = 32_000;
/// T8: lines of the first page of `read` on a large file, profile `local`.
const LOCAL_PAGE_LINES: u32 = 400;
/// T8: the same page on a small context.
const SMALL_PAGE_LINES: u32 = 200;
/// T8: bytes of the first page of `read` on profile `local`.
const LOCAL_PAGE_BYTES: u32 = 8192;
/// R5: the kept tail of a compaction on a small context, in tokens (when the user sets none).
pub(crate) const SMALL_KEEP_TOKENS: u32 = 2000;
/// R5: the least output cap of a small context, in bytes.
const SMALL_CAP_FLOOR: u32 = 2048;

/// Whether `n_ctx` is a known small context window (`0` = unknown, not small).
pub(crate) fn is_small(n_ctx: u32) -> bool {
    n_ctx > 0 && n_ctx < SMALL_CONTEXT
}

/// R5: the output cap of a small context in bytes, 12% of the window at about 3 bytes per
/// token, at least 2048. `None` when the context is not small.
pub(crate) fn small_output_cap(n_ctx: u32) -> Option<u32> {
    is_small(n_ctx).then(|| (n_ctx * 12 / 100 * 3).max(SMALL_CAP_FLOOR))
}

/// L4: the soft input of the byte budget, in tokens: the explicit `oxa.compaction.softTokens`
/// of the settings, else the engine's published compaction point (`compact_at`). A soft point
/// that the policy only derives from the window less the reserve is not an input. `0` when
/// neither is set (unknown).
pub(crate) fn budget_soft(st: &SessionState) -> u32 {
    st.settings
        .compaction()
        .soft_tokens
        .or(st.info.compact_at)
        .unwrap_or(0)
}

/// L4: the working size of a session in tokens, the smaller of the context window `n_ctx` and
/// the budget soft point `soft` ([`budget_soft`]), each ignored when it is 0 (unknown).
/// `0` when both are unknown.
pub(crate) fn working_tokens(n_ctx: u32, soft: u32) -> u32 {
    match (n_ctx, soft) {
        (0, s) => s,
        (n, 0) => n,
        (n, s) => n.min(s),
    }
}

/// L4: the byte budget of one tool result under profile `local`: 25% of the working size at
/// 3 bytes per token, at least [`SMALL_CAP_FLOOR`]. `None` when the working size is unknown.
pub(crate) fn budget_cap(working: u32) -> Option<u32> {
    (working > 0).then(|| (working / 4 * 3).max(SMALL_CAP_FLOOR))
}

/// L4: whether the user set `oxa.toolOutputCap` in some settings layer. Such a setting is the
/// output cap as it is (no budget applies).
fn cap_set(settings: &Settings) -> bool {
    settings.get("oxa.toolOutputCap").is_some()
}

/// The per-epoch tool tuning for `profile`, a context of `n_ctx` tokens (0 = unknown) and the
/// budget soft point `soft` ([`budget_soft`], 0 = unknown).
pub(crate) fn tune_for(profile: Profile, settings: &Settings, n_ctx: u32, soft: u32) -> ToolTune {
    let local = profile == Profile::Local;
    // L4: under `local` the first page of `read` is cut at most at 8 KiB within the output cap;
    // `grep`, `find`, `ls` and the other reads at the output cap itself.
    let cap = if local {
        u32::try_from(output_cap_for(profile, settings, n_ctx, soft)).unwrap_or(u32::MAX)
    } else {
        0
    };
    let (read_page_lines, read_page_bytes) = if !local {
        (0, 0)
    } else if is_small(n_ctx) {
        (SMALL_PAGE_LINES, LOCAL_PAGE_BYTES.min(cap))
    } else {
        (LOCAL_PAGE_LINES, LOCAL_PAGE_BYTES.min(cap))
    };
    ToolTune {
        diet: settings.diet_tools(profile),
        local,
        read_page_lines,
        read_page_bytes,
        bash_timeout_s: settings.bash_default_timeout(profile),
        output_cap_bytes: cap,
    }
}

/// Recompute `st.tune` from the epoch in force (`Stats::profile`: the profile of the frozen
/// prefix), the current settings and the engine's context window. Call it wherever one of those
/// changes without a new epoch (see the module doc).
pub(crate) fn refresh(st: &mut SessionState) {
    st.tune = tune_for(st.stats.profile, &st.settings, st.info.n_ctx, budget_soft(st));
}

/// R5 and L4: the output cap of `profile` for a context of `n_ctx` tokens and the budget soft
/// point `soft`. Under `local` it is the setting, reduced to the small-context cap (R5) and,
/// unless the user set `oxa.toolOutputCap`, to the budget of the working size ([`budget_cap`]).
pub(crate) fn output_cap_for(profile: Profile, settings: &Settings, n_ctx: u32, soft: u32) -> usize {
    let mut cap = settings.tool_output_cap();
    if profile != Profile::Local {
        return cap;
    }
    if let Some(small) = small_output_cap(n_ctx) {
        cap = cap.min(small as usize);
    }
    if !cap_set(settings) {
        if let Some(budget) = budget_cap(working_tokens(n_ctx, soft)) {
            cap = cap.min(budget as usize);
        }
    }
    cap
}

/// [`output_cap_for`] for the epoch in force (`Stats::profile`) and the session's context and
/// budget soft point.
pub(crate) fn output_cap(st: &SessionState) -> usize {
    output_cap_for(st.stats.profile, &st.settings, st.info.n_ctx, budget_soft(st))
}

/// R5: the kept tail in tokens. `local` on a small context keeps [`SMALL_KEEP_TOKENS`] unless
/// the user set `compaction.keepRecentTokens` (or the model override of it) in some layer
/// (`user_set`); otherwise `keep` is returned.
pub(crate) fn keep_tokens(profile: Profile, n_ctx: u32, user_set: bool, keep: u32) -> u32 {
    if profile == Profile::Local && is_small(n_ctx) && !user_set {
        SMALL_KEEP_TOKENS
    } else {
        keep
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn none() -> Settings {
        Settings::from_values(json!({}), json!({}))
    }

    #[test]
    fn tune_table() {
        let s = none();
        let pi = ToolTune::default();
        // pi: the default in every field, whatever the context.
        for n in [0, 16_384, 49_152] {
            assert_eq!(tune_for(Profile::Pi, &s, n, 0), pi, "pi at {n}");
        }
        // local, large or unknown context: the 400-line / 8 KiB page, no reduction.
        for n in [0, 49_152] {
            let t = tune_for(Profile::Local, &s, n, 0);
            assert!(t.local && t.diet);
            assert_eq!(
                (t.read_page_lines, t.read_page_bytes),
                (400, 8192),
                "local at {n}"
            );
            assert_eq!(t.bash_timeout_s, 120);
        }
        // local, small context: 200 lines, bytes = the small output cap (5898 at 16384).
        let t = tune_for(Profile::Local, &s, 16_384, 0);
        assert_eq!((t.read_page_lines, t.read_page_bytes), (200, 5898));
        // The byte page is never above 8192, and a tiny window keeps the 2048 floor.
        assert_eq!(tune_for(Profile::Local, &s, 31_999, 0).read_page_bytes, 8192);
        assert_eq!(tune_for(Profile::Local, &s, 1_000, 0).read_page_bytes, 2048);
        // The settings decide the diet and the timeout; pi stays at 0 unless set.
        let set = Settings::from_values(
            json!({"oxa": {"diet": {"tools": true}, "bash": {"defaultTimeout": 30}}}),
            json!({}),
        );
        let t = tune_for(Profile::Pi, &set, 0, 0);
        assert!(t.diet && !t.local);
        assert_eq!((t.read_page_lines, t.read_page_bytes), (0, 0));
        assert_eq!(t.bash_timeout_s, 30);
    }

    #[test]
    fn small_is_strictly_below_32000_and_known() {
        assert!(!is_small(0));
        assert!(is_small(1));
        assert!(is_small(31_999));
        assert!(!is_small(32_000));
    }

    #[test]
    fn output_cap_table() {
        let s = none();
        assert_eq!(output_cap_for(Profile::Local, &s, 16_384, 0), 5898);
        assert_eq!(output_cap_for(Profile::Local, &s, 49_152, 0), 16384);
        assert_eq!(output_cap_for(Profile::Local, &s, 0, 0), 16384);
        assert_eq!(output_cap_for(Profile::Pi, &s, 16_384, 0), 16384);
        let small_setting = Settings::from_values(json!({"oxa": {"toolOutputCap": 4000}}), json!({}));
        assert_eq!(output_cap_for(Profile::Pi, &small_setting, 16_384, 0), 4000);
        assert_eq!(output_cap_for(Profile::Local, &small_setting, 16_384, 0), 4000);
    }

    #[test]
    fn budget_table() {
        // The working size is the smaller known value of the window and the soft point.
        assert_eq!(working_tokens(131_072, 8_000), 8_000);
        assert_eq!(working_tokens(131_072, 0), 131_072);
        assert_eq!(working_tokens(0, 8_000), 8_000);
        assert_eq!(working_tokens(16_000, 100_000), 16_000);
        assert_eq!(working_tokens(0, 0), 0);
        assert_eq!(budget_cap(0), None);
        assert_eq!(budget_cap(2_000), Some(2_048));
        assert_eq!(budget_cap(8_000), Some(6_000));
        let s = none();
        // soft 8000 on a 131072 window: 6000-byte cap and first page.
        assert_eq!(output_cap_for(Profile::Local, &s, 131_072, 8_000), 6_000);
        let t = tune_for(Profile::Local, &s, 131_072, 8_000);
        assert_eq!((t.read_page_lines, t.read_page_bytes), (400, 6_000));
        assert_eq!(
            t.output_cap_bytes, 6_000,
            "grep, find, ls and explicit reads at the cap"
        );
        // soft 100000 on 131072: the setting (16 KiB) and the 8 KiB page.
        assert_eq!(output_cap_for(Profile::Local, &s, 131_072, 100_000), 16_384);
        let t = tune_for(Profile::Local, &s, 131_072, 100_000);
        assert_eq!((t.read_page_lines, t.read_page_bytes), (400, 8_192));
        assert_eq!(t.output_cap_bytes, 16_384, "only the first page is 8 KiB");
        // soft 2000: the floor.
        assert_eq!(output_cap_for(Profile::Local, &s, 131_072, 2_000), 2_048);
        // n_ctx 16000 and no soft point: the small-context numbers, unchanged.
        assert_eq!(output_cap_for(Profile::Local, &s, 16_000, 0), 5_760);
        let t = tune_for(Profile::Local, &s, 16_000, 0);
        assert_eq!((t.read_page_lines, t.read_page_bytes), (200, 5_760));
        assert_eq!(t.output_cap_bytes, 5_760);
        // pi ignores the soft point.
        assert_eq!(output_cap_for(Profile::Pi, &s, 131_072, 8_000), 16_384);
        assert_eq!(tune_for(Profile::Pi, &s, 131_072, 8_000), ToolTune::default());
        // An explicit setting is the cap as it is: no budget, the page is at most 8 KiB.
        let set = Settings::from_values(json!({"oxa": {"toolOutputCap": 12000}}), json!({}));
        assert_eq!(output_cap_for(Profile::Local, &set, 131_072, 8_000), 12_000);
        assert_eq!(
            tune_for(Profile::Local, &set, 131_072, 8_000).read_page_bytes,
            8_192
        );
        let small_set = Settings::from_values(json!({"oxa": {"toolOutputCap": 4000}}), json!({}));
        assert_eq!(output_cap_for(Profile::Local, &small_set, 131_072, 8_000), 4_000);
        assert_eq!(
            tune_for(Profile::Local, &small_set, 131_072, 8_000).read_page_bytes,
            4_000
        );
    }

    #[test]
    fn keep_tokens_table() {
        assert_eq!(keep_tokens(Profile::Local, 16_384, false, 4000), 2000);
        assert_eq!(keep_tokens(Profile::Local, 16_384, true, 4000), 4000);
        assert_eq!(keep_tokens(Profile::Local, 49_152, false, 4000), 4000);
        assert_eq!(keep_tokens(Profile::Pi, 16_384, false, 4000), 4000);
    }
}
