# Tools and context handling

oxa declares its built-in tools and shapes the context for the models the ox-llm engine
serves. The defaults below are the only ones; every one can be changed with a setting, and an
explicit setting always wins.

## The `edit` tool is declared flat

`edit` takes three string parameters:

```text
edit { path, oldText, newText }
```

A string parameter is written as raw text. An array would be JSON, so the model would have to
escape every line of code inside it. The flat form removes that escaping.

The tool still accepts the array form and the older forms, with the same result text, error
texts and file handling. The tool description asks the model to make one `edit` call per
change, in the same reply. The calls run in order, and each call matches the file as the
previous one left it.

| Case | Behaviour |
|---|---|
| one replacement | one call |
| several disjoint replacements | several calls in one reply, same path, run in order |
| a failed replacement | a failed call changes nothing; calls before it stay applied, and each call reports its own result |
| overlapping replacements | the later call gets the not-found text, because the earlier call replaced that text |
| no exact match | the not-found error, unless the text matches once when indentation is ignored; then that match is used. The error names the closest text in the file |
| BOM, CRLF, mixed line endings | preserved |

One limit applies because of the engine's parameter syntax: a line that is exactly
`</parameter>` cannot be passed in a parameter. The same text inside a longer line is fine.

## Thinking follows the kind of turn

`oxa.thinking.policy` chooses when thinking is on for a request:

| Policy | Behaviour |
|---|---|
| `fixed` | every request carries the session's thinking level |
| `auto` (default) | thinking is on for the first reply after each user prompt; tool follow-ups follow `oxa.thinking.toolFollowups` (default `auto`: off for routine steps, on after a failed call, repeat note or several calls without progress; see [tools.md](tools.md#thinking-on-tool-follow-ups)); it is off for turns after an appended instruction, for summaries and for background requests |

The thinking level `off` always wins: with `off` no request thinks, whatever the policy.

On an engine model that does not keep reasoning in its history, `auto` behaves as `fixed`.
Turning thinking on and off changes the start of the prompt for those models, so toggling per
turn would re-process the whole context. Subagent children read the same settings, so they
follow the same rule.

An optional `{"oxa":{"thinking":{"budget":512}}}` setting bounds generated reasoning
text on supporting local oxe engines. It is sent only on requests whose thinking policy is
on; `off`, prefill-only, unknown and remote engines omit it. At the cap oxe evaluates the
closing think marker and continues into the answer/tool phase within the same total output
limit. Omitted/null preserves the existing behavior; zero closes immediately. This limits
reasoning rather than promising a correct or complete answer. The closing marker counts in
usage, so reported reasoning tokens can be one greater than the text cap.

## Defaults for the tools and the context

- **bash timeout.** A bash call with no `timeout` stops after 120 seconds.
  `oxa.bash.defaultTimeout` sets the value; `0` means none.
- **Shorter first page of `read`.** A `read` with no `offset` or `limit` returns the first 400
  lines, and at most 8 KiB. On a model with a context below 32K tokens it returns 200 lines and
  the byte cap is lower. The first page ends with a note that says how to continue, and
  suggests `grep` to find a part.
- **Output cap on a small context.** On a model with a context below 32K tokens, the cap on one
  tool output is set from the context window: about 12% of the window, at roughly three bytes
  per token, and at least 2 KiB. A larger context keeps the cap of `oxa.toolOutputCap`.
- **Kept tail after a compaction.** On a small context, the recent messages kept after a
  compaction are about 2000 tokens, unless `compaction.keepRecentTokens` is set. See
  [compaction.md](compaction.md).
- **Sized by the prefill speed.** On a local model, oxa measures the prefill speed of each
  reply that prefilled at least 256 new tokens. One hop (a tool result, or the kept tail
  after a compaction) is then kept to what prefills in `oxa.tune.hopSeconds` (10 seconds
  unless set; `0` turns it off): the output cap and the first page of `read` at three bytes
  per token, never below 2 KiB, and the kept tail in tokens, never below 2000. This only
  lowers the values above, never raises them. Before the first measurement, on a cloud model
  and when `oxa.toolOutputCap` or `compaction.keepRecentTokens` is set, the values above apply
  unchanged.
- **Shaped output.** `oxa.diet.tools` is on by default. It makes `grep` print each file's path
  once, with its matches below it, and it collapses runs of three or more blank lines in bash
  output to two.
- **Budget of a tool result.** When `oxa.compaction.softTokens` is set or the engine publishes a
  compaction point, one tool result is at most a quarter of it (three bytes per token, never
  below 2048 bytes), unless `oxa.toolOutputCap` is set. Only the first page of a `read` (no
  `offset` or `limit`) is at most 8 KiB; `grep`, `find`, `ls` and a `read` with `offset` or
  `limit` are cut at the cap itself (16 KiB by default on a large context), never above
  16 KiB. A cut ends with a note inside the cap: the next `offset` for `read`, a narrower
  search for `grep`, `find` and `ls`.
- **Count form of a large `grep`.** When the matches are more than the `limit`, `grep` answers
  with the total and one count per file (`<path>: <n> matches`) instead of the lines. Narrow
  the pattern, path or glob, or raise limit, to see the lines.
- **Conciseness rule.** The system prompt asks for short tool use and a final answer of at most
  6 lines, in place of its "be concise" rule (`oxa.diet.output`, on by default).
- **No `<tools>` section.** The chat template renders the tool list from the tool JSON, so the
  system prompt leaves the `<tools>` section out.
- **Summary length.** A compaction summary is asked for about 550 words (a target of 700
  tokens, three quarters of it in words). `oxa.compaction.summaryTarget` sets it; `0` is no
  target. See [compaction.md](compaction.md#summary-length-and-idle-summary).
- **Idle summary and mid-run compaction.** The summary of the next compaction is written in idle
  time, and a long run is compacted between tool steps when the context passes the compaction
  point (`oxa.compaction.idleSummary`, `oxa.compaction.midRun`). See
  [compaction.md](compaction.md).
- **Prefill.** While you type, a draft message is prefilled; while a long tool call runs, the
  results before it are prefilled (`oxa.prefill.paste`, `oxa.prefill.tools`; timing only, the
  bytes do not change).
- **Date and environment.** Each prompt you send ends with a short environment note
  (`oxa.envNote`): the first one in a session carries the date, time zone, model, context size
  and permission mode, later ones only the local time. The note is appended to your message,
  never put in the system prompt, so earlier requests stay byte-identical and the cache is reused.

## If no summary can be made

When the summary of a compaction fails, or is cancelled twice in a row, oxa does not stop the
session. It writes a mechanical record instead: the user's messages in order, and the files
read and changed. Assistant replies and tool results are left out. A warning notice says that
the record was built without the model, and the compaction entry is marked as mechanical.

## Tool output and the wire

Tool text a model caused is capped and deterministic: the same arguments and file state give
the same bytes, with no timings and no terminal escapes. Everything a model sees from a tool
passes through the same shaper, whether the tool is built in, an extension, an MCP server or
codemode.
