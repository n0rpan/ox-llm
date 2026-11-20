//! The `Sessions and compaction` rows of the settings page, in page order (issue #2042, R3): the docs view of
//! the descriptor table, rendered into `oxa/docs/pages/settings.md` by
//! `tools/ci/settings_table.py`. Edit a row here and rerun the tool.

use super::{row, PageRow};

/// The section heading shown on the page.
pub(super) const TITLE: &str = "Sessions and compaction";

#[rustfmt::skip]
pub(super) const ROWS: &[PageRow] = &[
    row("sessionDir", "string", "`null`", "session storage; `--session-dir` and `OXA_SESSION_DIR` win over it"),
    row("compaction.enabled", "bool", "`true`", "compact automatically; `false` keeps only the emergency compaction"),
    row("compaction.reserveTokens", "number", "`16384`", "room kept free for the summary and the next reply"),
    row("compaction.keepRecentTokens", "number", "`4000`", "recent context kept verbatim after a compaction"),
    row("compaction.modelOverrides", "object", "`null`", "per-model `reserveTokens` and `keepRecentTokens`"),
    row("compaction.profile", "`auto`, `local` or `cloud`", "`auto`", "`auto` takes `cloud` for a cloud model and `local` otherwise. `cloud` compacts at 90 % of the window, keeps 20000 tokens verbatim and writes a summary of about 2000 tokens. A `compaction.keepRecentTokens`, `oxa.compaction.summaryTarget` or `oxa.compaction.softTokens` set in any layer wins over the profile. `local` keeps the values above"),
    row("compaction.reactivateSkills", "bool", "`false`", "retain invoked skills across compaction and ask the model to re-read them. User settings only"),
    row("oxa.compaction.idleSummary", "bool", "unset", "after a run that leaves the context between 80 % and 100 % of the soft limit, the summary of the next compaction is written in idle time, and that compaction reuses it with no model request if nothing it covers has changed. Unset is on"),
    row("oxa.compaction.midRun", "bool", "unset", "compact between tool steps of a run when the context passes the compaction point; unset is on"),
    row("oxa.compaction.summaryTarget", "number (tokens)", "unset", "soft length target of a compaction summary: the model is asked for about three quarters of it in words; `0` is no target. Unset is 700"),
    row("oxa.compaction.softTokens", "number", "`null`", "context size that starts an idle compaction; unset uses the engine's point"),
    row("oxa.compaction.turnStart", "bool", "unset", "compact before a prompt is sent when the context is already above the compaction point (for example after an aborted turn); unset is on"),
    row("branchSummary.reserveTokens", "number", "`16384`", "output budget of a branch summary"),
    row("branchSummary.skipPrompt", "bool", "`false`", "leave out the \"summarise the branch?\" question"),
];
