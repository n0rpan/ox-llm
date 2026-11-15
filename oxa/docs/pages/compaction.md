# Compaction and the cache

A local engine has one context window and handles one request at a time. oxa keeps a conversation inside that window by compacting it: older messages are replaced by a summary, and the most recent ones are kept as they were. Compaction also changes the bytes the engine has cached, which costs a re-prefill. This page explains when it happens, what is kept, and what the cache notices mean.

## The limits

oxa reads the limits from the engine when it opens a session, when you switch model, and on `/reload`:

| Value | Source |
|---|---|
| context window `n_ctx` | the engine's limits for the selected model |
| output limit `max_output` | the output limit the engine publishes for the model; if it publishes none, the smaller of 16384 and a quarter of `n_ctx` |
| hard limit | `n_ctx - max_output - compaction.reserveTokens`, and never below 1024 |
| soft limit | the compaction point the engine reports for the selected mode, unless `oxa.compaction.softTokens` is set; when the engine reports none, 75 % of the hard limit |
| summary output | at most `max_output`, and at most `compaction.reserveTokens * 4 / 5` |

The soft limit is always below the hard limit.

## When it runs

There are two triggers.

1. **After a reply (soft limit).** When a run ends and the context is above the soft limit, oxa compacts in idle time. The summary request runs in the background and can be cancelled by your next input. If it is cancelled, the conversation is unchanged and the decision is taken again after the next run.
   With `oxa.compaction.turnStart` on by default, a prompt that finds the context already above the soft limit (for example after an aborted turn, whose large tool result was never counted) is compacted in the foreground before it is sent.
   With `oxa.compaction.midRun` on by default, a run whose context passes the soft limit is compacted in the foreground between two tool steps, before its next request, and the tool calls and results the model has not answered yet are kept verbatim.
2. **During a run (hard limit or overflow).** When the context is above the hard limit, or the engine reports that the request does not fit, oxa compacts in the foreground and then retries the request once. This happens also when `compaction.enabled` is `false`, because otherwise the request would fail.

`/compact [instructions]` compacts now. Any text after the command is added to the summary request as guidance, for example `/compact keep the list of open questions`.

Switching to a model with a smaller window is checked first. If the conversation would not fit in the new model's hard limit, oxa asks you to choose: compact first on the current model, fork the session, or cancel. Nothing changes until you choose.

## What is kept

A compaction keeps three things:

- A **summary** of the older messages. It is written by the session's own model with the same frozen prefix (the system prompt and the tool list), so the summary request reuses the cache. The summary lists the files that were read and changed; these lists accumulate over several compactions.
- The **kept tail**: the most recent messages, up to `compaction.keepRecentTokens` tokens (4000 by default), verbatim. A tail starts at a message boundary.
- The **frozen prefix**: the system prompt and the tool declarations. They are not changed by a compaction.

A tool result inside the kept tail that is larger than half of `keepRecentTokens` (and at least 1000 tokens) is replaced by a marker of the form `[output elided at compaction: N bytes; ...]`. The summary covers its content, and the marker keeps the rest of the tail from being filled by one large file. The original stays in the session file.

The new context is the summary as a user message, followed by the kept tail. The session file keeps every message: a compaction adds an entry and does not delete the older ones.

## The cache

The engine keeps the processed prefix of the conversation in a cache. A request that continues the same conversation only processes the new tokens. Three things break that:

- a compaction rewrites the messages after the frozen prefix,
- moving the leaf in `/tree` or forking changes the path,
- a model switch, `/reload`, or a change to skills, context files or tools starts a new prefix epoch.

After each of these the next request processes the new head again. oxa expects this, and sends a short idle request to prefill the new head when the engine supports it. Turn this off with `cacheWarming: "off"`. The default `"idle"` does the prefill in idle time; `"streaming"` is accepted and works the same way.

After the head has been processed once, every further request that only appends to it continues from the cache.

### Cache misses

oxa checks each reply. If the engine reports fewer cached tokens than the request should have reused, it records a cache miss:

```text
Cache miss (engine_evicted): 1792 of 11838 tokens cached
```

The text is `Cache miss (<cause>): <cached> of <expected> tokens cached`. The causes:

| Cause | Meaning |
|---|---|
| `compaction` | the context was rewritten by a compaction |
| `leaf_move` | the leaf moved in `/tree` |
| `context_edit` | an extension changed the context |
| `epoch` | the frozen prefix changed: a model switch, `/reload`, or a changed tool or skill list |
| `extension` | an extension request changed the prefix |
| `cold_start` | the first request of this process on a session the engine does not hold |
| `engine_evicted` | the engine dropped the session's cached state: its cache was full, it restarted, or another session used the slot |

A miss with an unknown cause should not occur. The footer counts such misses separately. The `cache_miss` event is always written to the JSON and RPC streams. The notice in the interface is shown only when `showCacheMissNotices` is `true`, which is the default `false`. The footer's `cache cached/prompt` figure is the same count for the last request.

A miss after a compaction or a leaf move is expected once. A miss with cause `engine_evicted` in the middle of a conversation means the engine lost the state, and the next request will rebuild it.

## Summary length and idle summary

Two settings change how the summary is made ([tools-context.md](tools-context.md)).

- **`oxa.compaction.summaryTarget`** (tokens). The summary request asks the model for about three quarters of this many words. It is a request, not a limit: the summary is still at most the output limit above. `0` is no target. Unset is 700 tokens (about 550 words).
- **`oxa.compaction.idleSummary`**. When a run ends with the context between 80 % and 100 % of the soft limit, oxa writes the summary of the next compaction in idle time. That compaction then uses it with no model request, if the session, the frozen prefix, the model and the messages it covers are unchanged and the kept tail is not too large. Otherwise the summary is dropped and the compaction runs as usual. A cancelled or failed idle summary is silent.

### If no summary can be made

When the summary of a compaction fails, or is cancelled twice in a row, oxa does not stop the session. It writes a mechanical record instead: the user's messages in order, and the files read and changed. Assistant replies and tool results are left out. A warning notice says that the record was built without the model, and the compaction entry is marked as mechanical.

## Settings

| Setting | Default | Effect |
|---|---|---|
| `compaction.enabled` | `true` | automatic compaction after a reply; the emergency compaction stays on |
| `compaction.reserveTokens` | `16384` | room kept free for the summary and the next reply |
| `compaction.keepRecentTokens` | `4000` | size of the kept tail |
| `compaction.modelOverrides` | `null` | per model: `{"<model-id>": {"reserveTokens": n, "keepRecentTokens": n}}` |
| `oxa.compaction.softTokens` | unset | context size that starts the idle compaction |
| `oxa.compaction.idleSummary` | unset (on) | writes the next summary in idle time |
| `oxa.compaction.summaryTarget` | unset (700) | soft length target of the summary, in tokens |
| `cacheWarming` | `"idle"` | `"off"` disables the idle prefill |
| `showCacheMissNotices` | `false` | show a notice for each cache miss |
| `branchSummary.reserveTokens` | `16384` | output budget of a branch summary |

A `compaction.modelOverrides` entry that names another model for the summary is ignored with a warning. The summary always runs on the session's model, because only one model is loaded at a time.

Subagents compact on their own context. Their soft limit is the smaller of `oxa.subagent.softTokens` (24000), the parent's compaction point, and the child's window minus one ([tools.md](tools.md#subagent)).

## Example

With a 32768-token window, the default output limit (8192, a quarter of the window), and the default reserve:

```text
hard = 32768 - 8192 - 16384 = 8192   (never below 1024)
soft = 75 % of hard = 6144           (unless the engine reports its own point)
```

A run that ends at 7000 tokens compacts in idle time. A run that reaches 8200 tokens compacts in the middle of the run and retries. The summary is at most 8192 tokens, and the kept tail is at most 4000 tokens.

If your engine reports its own compaction point, that point is the soft limit and the rest of the arithmetic is the same.
