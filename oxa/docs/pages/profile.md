# Profiles

A profile sets the formats and defaults oxa uses for a model. Both profiles have the same tools, permission modes, commands, sessions, MCP support, extensions and print or RPC output. They differ in the tool formats, in some defaults that suit small local models, and in a few limits on tool output.

| Profile | Use it for |
|---|---|
| `pi` | the formats the earlier agent used. Select it explicitly for engines and models that expect that shape |
| `local` (default) | the local ox-llm engine and its models. Formats and defaults are tuned to the local engine and its models |

## Selecting a profile

The default is `local`. Set another in a settings file:

```json
{ "oxa": { "profile": "local" } }
```

in `~/.oxa/agent/settings.json`, or in the project's `.oxa/settings.json` (the project file wins, if the project is trusted). The environment variable `OXA_PROFILE=pi` (or `=local`) wins over both settings files. Any value other than `pi` or `local` is reported as a warning that names both values, and the next layer decides.

A session keeps the profile it was created with. A change of profile takes effect at the next prefix epoch: after `/reload` (when the change alters the system prompt or the tool list), after a model switch, or in a new session. `/reload` opens a new epoch when only the profile changed, because the declared tool text is different.

The profile in force is shown by the RPC commands `get_state` and `get_session_stats` (field `profile`). The interface header shows `local` after the version under the `local` profile.

## What `local` changes

### 1. The `edit` tool is declared flat

Under `pi`, `edit` takes an array of replacements:

```text
edit { path, edits: [ { oldText, newText }, ... ] }
```

Under `local` it takes three strings:

```text
edit { path, oldText, newText }
```

The reason is the tool-call format of the local models. A string parameter is written as raw text. An array is JSON, so the model has to escape every line of code inside it. The flat form removes that escaping.

The tool still accepts the array form and the older forms in both profiles, with the same result text, error texts and file handling. Under `local`, the tool description asks the model to make one `edit` call per change, in the same reply. The calls run in order, and each call matches the file as the previous one left it.

| Case | `pi` (`edits[]`) | `local` (flat) |
|---|---|---|
| one replacement | one call | one call, same result |
| several disjoint replacements | one call with several `edits` | several calls in one reply, same path, run in order |
| a failed replacement | nothing from that call is applied | a failed call changes nothing; calls before it stay applied, and each call reports its own result |
| overlapping replacements | an error | the later call gets the not-found text, because the earlier call replaced that text |
| no exact match | the not-found error | the same error, unless the text matches once when indentation is ignored; then that match is used. The error names the closest text in the file |
| BOM, CRLF, mixed line endings | same code path | same code path |

One limit applies in both forms, because of the engine's parameter syntax: a line that is exactly `</parameter>` cannot be passed in a parameter. The same text inside a longer line is fine.

### 2. Thinking follows the kind of turn

`oxa.thinking.policy` chooses when thinking is on for a request:

| Policy | Behaviour |
|---|---|
| `fixed` | every request carries the session's thinking level |
| `auto` | thinking is on for the first reply after each user prompt; tool follow-ups follow `oxa.thinking.toolFollowups` (default `auto`: off for routine steps, on after a failed call, a repeat note or several calls without progress; see [tools.md](tools.md#thinking-on-tool-follow-ups)); it is off for turns after an appended instruction, for summaries and for background requests |

The default is `fixed` under `pi` and `auto` under `local`. An explicit `oxa.thinking.policy` wins in both profiles. The thinking level `off` always wins: with `off` no request thinks, whatever the policy.

On an engine model that does not keep reasoning in its history, `auto` behaves as `fixed`. Turning thinking on and off changes the start of the prompt for those models, so toggling per turn would re-process the whole context. Subagent children read the same settings and environment, so they follow the same rule.

### 3. Defaults for bash, read and grep

- **bash timeout.** Under `pi` a bash call has no timeout unless it names one. Under `local` a call with no `timeout` stops after 120 seconds. The setting `oxa.bash.defaultTimeout` sets the value for either profile; `0` means none.
- **Shorter first page of `read`.** A `read` with no `offset` or `limit` returns the first 400 lines, and at most 8 KiB. On a model with a context below 32K tokens it returns 200 lines and the byte cap is lower. The first page ends with a note that says how to continue, and suggests `grep` to find a part. Under `pi` `read` returns up to 2000 lines or 16 KB, as described in [tools.md](tools.md#the-built-in-tools).
- **Output cap on a small context.** On a model with a context below 32K tokens, the cap on one tool output is set from the context window: about 12% of the window, at roughly three bytes per token, and at least 2 KiB. A larger context keeps the cap of `oxa.toolOutputCap`.
- **Kept tail after a compaction.** On a small context, the recent messages kept after a compaction are about 2000 tokens, unless `compaction.keepRecentTokens` is set. See [compaction.md](compaction.md).
- **Shaped output.** `oxa.diet.tools` is on by default under `local` and off under `pi`. It makes `grep` print each file's path once, with its matches below it, and it collapses runs of three or more blank lines in bash output to two.
- **Budget of a tool result.** Under `local`, when `oxa.compaction.softTokens` is set or the engine publishes a compaction point, one tool result is at most a quarter of it (three bytes per token, never below 2048 bytes), unless `oxa.toolOutputCap` is set. Only the first page of a `read` (no `offset` or `limit`) is at most 8 KiB; `grep`, `find`, `ls` and a `read` with `offset` or `limit` are cut at the cap itself (16 KiB by default on a large context), never above 16 KiB. A cut ends with a note inside the cap: the next `offset` for `read`, a narrower search for `grep`, `find` and `ls`.
- **Count form of a large `grep`.** Under `local`, when the matches are more than the `limit`, `grep` answers with the total and one count per file (`<path>: <n> matches`) instead of the lines. Narrow the pattern, path or glob, or raise limit, to see the lines. Under `pi` the first matches up to the limit are shown, with a note.

### 4. The prompt, compaction and prefill

- **Conciseness rule.** Under `local` the system prompt asks for short tool use and a final answer of at most 6 lines, in place of its "be concise" rule (`oxa.diet.output`, on by default under `local`).
- **Summary length.** Under `local` a compaction summary is asked for about 550 words (a target of 700 tokens, three quarters of it in words). Under `pi` there is no target. `oxa.compaction.summaryTarget` sets it for either profile. See [compaction.md](compaction.md#summary-length-and-idle-summary).
- **Idle summary.** Under `local` the summary of the next compaction is written in idle time when the context is between 80 % and 100 % of the soft limit, and reused when nothing it covers has changed (`oxa.compaction.idleSummary`).
- **Compaction during a run.** Under `local` a run whose context passes the soft limit is compacted between two tool steps, before its next request, keeping the unanswered tool calls and results verbatim (`oxa.compaction.midRun`).
- **Mechanical record.** Under `local`, when a compaction cannot get its summary, because the request fails or is cancelled twice in a row, oxa writes a record built without the model: the user's messages in order and the files touched. A warning says so, and the session goes on. Under `pi` the failure is an error. See [compaction.md](compaction.md#if-no-summary-can-be-made).
- **Date and environment.** Under `local` each prompt you send ends with a short environment note (`oxa.envNote`): the first one in a session carries the date, time zone, model, context size and permission mode, later ones only the local time. The note is appended to your message, never put in the system prompt, so earlier requests stay byte-identical and the cache is reused.
- **Prefill.** Under `local` oxa prefills a long draft while you type it (`oxa.prefill.paste`), and the results of the calls before a slow tool call while that call runs (`oxa.prefill.tools`). Both need an engine that supports prefill-only requests; without one they do nothing.

These defaults are the only ones that differ. A setting that is set explicitly always wins over the profile's default.

## What stays the same

Everything else is identical in both profiles: the seven built-in tools and what they do, permission modes and the sandbox, the cache rules, sessions and the tree, skills, extensions, MCP, `web_search`, and the print, JSON and RPC modes. The command-line options do not change, and the settings keep their meaning.

## Choosing

Use `local` when the engine serves the ox-llm local models. Use `pi` when the engine serves another model, or when you want the array form of `edit` and thinking on for every request. Both work with the same settings file; only the value changes.

To see the profile a running process uses, send `get_state` over RPC and read the `profile` field ([rpc.md](rpc.md)).
