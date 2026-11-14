# Engine support

oxa talks to one engine slot: one model request is in flight at a time for the whole
instance. A request that arrives while the slot is busy queues, and the screen shows its
position. Subagents run one at a time, never in parallel, because they share the slot.
The engine is text-only: image, audio and binary content never reach it, and MCP results
of those kinds become a fixed placeholder line. Thinking can be turned on or off per
request. Context window, compaction threshold and maximum output come from the engine,
per model, as the engine reports its limits. The small Flash model has reduced features:
a 16K context, no preserved thinking, and no subagent tool unless it is enabled for it.
The table is generated from `PARITY.md` by `ci/gen_docs.py`. Each row has an id in the first
column, of the form `AREA-NNN`: the row's key in oxa's internal parity table, shown so that a row can be referred
to. The text of the rows does not cite the internal work items.

## Engine rows

| id | behaviour | note |
|---|---|---|
| ENG-001 | One oxa instance never has two model requests in flight: the run, the summary, the idle prefill and extension / codemode model calls pass one gate | one engine slot; a second request would only queue and park the first one's state |
| ENG-002 | Input from the user (a queued steering or follow-up message, an abort) cancels this instance's own in-flight background request: the threshold compaction summary and the idle prefill. A cancelled prefill is skipped silently; a cancelled summary is decided again after the next run; the queued input then runs (`Session:prompt` before it returns, `Session:run_queued` after a navigation or `/compact`). Summaries the user waits on (`/compact`, overflow recovery, the compaction before a model switch, a branch summary) are foreground and end only by an abort | orders the queue but does not cut a running request; the engine keeps the prefill progress of a cancelled request |
| ENG-003 | Engine busy with another client: the `: queued [pos=<n>] wait_s=<s>` comments (and the two older forms) are shown as `queued #<pos>` or `queued <s>s` (activity line and footer; `engine_queue` record `{pos, waitS}` in JSON / RPC), never time out while they arrive, and do not change the exit code. An error after the early `200` (SSE `error` event) is classified like a status: overflow -> compaction, 4xx final, 5xx retried; `503` (`engine_busy`) is retried after its `Retry-After` with the same `Idempotency-Key` | controller queue (round-robin per session, cap 8 waiters per principal) |
| ENG-004 | `/model`, `set_model`, the cycle key and extension `setModel` to a model whose hard limit (`cap - max_output - reserveTokens`) is below the current context: nothing is sent to the target above it. Interactive: the user sees both numbers and chooses compact first / fork / cancel. Everywhere else (RPC, extensions): compact first. Compact first = a foreground summary on the SOURCE model, sized for the target; the switch happens only when the rebased head fits, else the model is unchanged | Flash 16K vs 128K; the switch itself is a new epoch whose one notice prints the cold re-prefill tokens and estimated seconds for the whole wire |
| ENG-005 | Context window, compaction threshold and maximum output come from `/_local/status` `model_limits[<model>]`; no per-model table in oxa; an engine without it uses the provisional minimum rule | per-alias limits |
| ENG-006 | Every request names its session (`X-Session-Id`); idle prefill, the park and `X-Oxe-Snapshot: keep` are sent only when advertised: `model_limits[<model>].capabilities` (known before the model loads), else `/props` `oxe_hints`, re-read once after the first reply and after a recovered `503`. `preserve_thinking` is not sent to a model without that capability (Flash). A null `snap_pos` is one info notice | parked sessions and round-robin are keyed by the session; an engine without would sample a token |
| ENG-010 | `subagent`: at most one child process alive; `tasks` run in call order and show as `subagent k/n, queued` | one engine slot: parallel children would only queue and park each other |
| ENG-011 | At most eight children per parent session over the whole tree: more than 8 `tasks` or `chain` steps are refused, and children that run or wait are counted over every call in flight; a refusal is a tool error and starts nothing | pi caps only the `tasks` array |
| ENG-012 | Each child sends `X-Session-Id: <parent>.s<n>` (n counts up per parent session), `X-Oxe-Priority: background` and turn kind `subagent` | the engine parks and restores per session; a human at another prompt goes first |
| ENG-013 | Depth 1: a child never loads `builtin:subagent` | a nested child would park its parent's child |
| ENG-014 | An agent's `model:` other than the session's is ignored with a warning; the child runs on the session's model | one model loaded; a swap cold-prefills the whole wire |
| ENG-015 | Child soft compaction limit = min(`oxa.subagent.softTokens` 24000, the parent's `compact_at`, cap - 1); the child compacts like any session | each child prefills its own context, so a call with children takes longer than one prompt |
| ENG-016 | The tool result is each child's final assistant text only, capped at `oxa.subagent.resultCap` (6 KiB) with a note naming the spill file | the result is prefilled at the parent's depth (no timings) |
| ENG-017 | The tool is not declared on a model whose context is below 32000 unless `oxa.subagent.smallContext`; re-evaluated at a model switch | Flash: 16K context, disk parks only |
| ENG-018 | Before the first child of a call the parent is parked: one zero-token request with `X-Oxe-Snapshot: keep` (skipped when the engine does not advertise it); the parent's next request reuses exactly the parked tokens. After every child, also a failed or aborted one, its engine state is cleared by its hint; the parent's state and shared blocks stay | the park and the hint clear use the snapshot and clear-by-hint requests |
| ENG-019 | Abort or shutdown kills the child's process group (TERM, then KILL after 2 s) and starts no further task; the closed connection cancels the child's engine request | cancel propagation |

## Other rows that depend on the one-slot engine

| id | behaviour | note |
|---|---|---|
| CMP-015 | `compaction.modelOverrides` per model | engine model ids. an override naming another model than the session's is ignored with a warning; the summary runs on the session model (one slot: a swap unloads the model and cold-prefills the whole wire on the other one) |
| MCP-015 | Resource tools `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` | text-only engine: image / audio / binary content blocks of any MCP result become the fixed placeholder line |
| MCP-026 | MCP `sampling/createMessage` (a server asks the client's model) | the engine has one inference slot and a server may not take it; the capability is not advertised and the request gets JSON-RPC -32601; the manager never calls the engine |
| EXT-093 | Example ports: plan-mode, permission-gate, custom-compaction, todo, status-line | each loads and passes a scripted session. pi's custom-compaction summarises with a second (cloud) model; the port sends no model request of its own (one model, one slot) and supplies custom instructions to the summary request |
