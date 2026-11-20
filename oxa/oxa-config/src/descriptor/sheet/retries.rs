//! The `Retries` rows of the settings page, in page order (issue #2042, R3): the docs view of
//! the descriptor table, rendered into `oxa/docs/pages/settings.md` by
//! `tools/ci/settings_table.py`. Edit a row here and rerun the tool.

use super::{row, PageRow};

/// The section heading shown on the page.
pub(super) const TITLE: &str = "Retries";

#[rustfmt::skip]
pub(super) const ROWS: &[PageRow] = &[
    row("retry.enabled", "bool", "`true`", "retry a failed request; off sends each request once, with no provider retry"),
    row("retry.maxRetries", "number", "`10`", "retries of an agent request"),
    row("retry.baseDelayMs", "number", "`2000`", "first delay, doubled each retry"),
    row("retry.maxAgentDelayMs", "number", "`60000`", "longest delay the agent loop waits"),
    row("retry.localMaxTotalMs", "number", "`5000`", "whole retry wait when this machine's own engine fails: an engine that is down or keeps answering the same error does not recover by waiting, so oxa gives up after this budget and names the endpoint and how to restart it (`0` gives up at once). A cloud model through the local gateway is not local and keeps the long backoff"),
    row("retry.identical5xxRetries", "number", "`2`", "retries of the same plain 5xx body (status and message) from one endpoint: an identical answer will not change, so the request stops after it. `503`, `529`, a reply that names a wait (`retry after <n> s`) and overload markers keep the long backoff; `0` retries no identical body"),
    row("retry.provider.maxRetries", "number", "`0`", "silent retries of one HTTP request, before the first byte of the reply; `retry.enabled` false disables them"),
    row("retry.provider.maxRetryDelayMs", "number", "`60000`", "longest wait of one HTTP retry"),
    row("retry.provider.timeoutMs", "number", "`null`", "silence limit of the engine stream; unset uses `httpIdleTimeoutMs`"),
];
