//! The `Models and thinking` rows of the settings page, in page order (issue #2042, R3): the docs view of
//! the descriptor table, rendered into `oxa/docs/pages/settings.md` by
//! `tools/ci/settings_table.py`. Edit a row here and rerun the tool.

use super::{row, PageRow};

/// The section heading shown on the page.
pub(super) const TITLE: &str = "Models and thinking";

#[rustfmt::skip]
pub(super) const ROWS: &[PageRow] = &[
    row("defaultModel", "string", "`null`", "model to start with, when the engine serves it"),
    row("enabledModels", "list", "`null`", "model patterns for the Ctrl+P cycle and the start model of a new session"),
    row("defaultThinkingLevel", "`off` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max`", "`medium`", "thinking level of a new session"),
    row("modelThinkingLevels", "object", "`{}`", "thinking level per model id, for example `{\"your-model-id\": \"low\"}`"),
    row("oxa.thinking.budget", "non-negative integer / null", "unset", "optional reasoning-text token cap, only while thinking is on and a local engine advertises `thinking_budget`; `0` closes reasoning immediately. Unknown and remote engines omit this local field. The total output limit still applies; no default changes ([thinking policy](tools-context.md))"),
    row("oxa.thinking.policy", "`fixed` / `auto`", "unset", "when thinking is on; unset is `auto` ([tools-context.md](tools-context.md))"),
    row("oxa.thinking.toolFollowups", "`auto` / `on` / `off`", "`auto`", "thinking on the replies after tool results, when `oxa.thinking.policy` is `auto`. `off`: never. `on`: always. `auto`: off for routine steps, on while the run needs reasoning: after a failed call (non-zero exit, an edit that did not apply, a failing test or build), after a repeat notice, or after `oxa.thinking.escalation.fruitlessCalls` calls without progress. It goes back off after a successful edit or write or a passing test or build ([tools.md](tools.md#thinking-on-tool-follow-ups)). The level `off` always wins"),
    row("oxa.thinking.escalation.fruitlessCalls", "number", "`6`", "under `oxa.thinking.toolFollowups: auto`, the calls without an edit or write since the prompt that turn thinking on (re-reads, repeated calls, commands; reading a new file or range is progress and not counted); `0` = never on this ground"),
    row("cacheWarming", "`off` / `streaming` / `idle`", "`idle`", "`off` disables warm-up; `streaming` is accepted and behaves as `idle`"),
    row("oxa.bootPreload", "bool", "`true`", "prefill the frozen prefix when oxa starts, if the engine supports it"),
    row("oxa.prefill.paste", "bool", "unset", "while you type a message of at least 2000 bytes and stop for 0.7 seconds, with oxa idle, the engine prefills it so Enter starts sooner; only sent to an engine on this machine, and it needs an engine that supports prefill-only requests. Unset is on"),
    row("oxa.prefill.tools", "bool", "unset", "while a tool call that runs alone takes more than 2 seconds, the engine prefills the results of the calls before it, once they total at least 2048 bytes; needs an engine that supports prefill-only requests. Unset is on"),
    row("oxa.sampling.mode", "`greedy` | `model`", "`greedy`", "`greedy` sends `temperature: 0` (deterministic; the engine's speculative decoding needs it). `model` sends the sampling the model card recommends (Qwen3.6 and Ornith: temperature 0.6, top_p 0.95, top_k 20 with thinking on; 0.7, 0.8, 20 and presence_penalty 1.5 with thinking off) for chat and subagent replies; other models stay greedy"),
    row("oxa.sampling.seed", "number", "unset", "a fixed seed sent with `oxa.sampling.mode: model`"),
];
