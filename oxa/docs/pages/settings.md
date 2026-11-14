# Settings

Settings are JSON. oxa reads two files:

| File | Scope |
|---|---|
| `~/.oxa/agent/settings.json` | user settings, for you on this machine |
| `<project>/.oxa/settings.json` | project settings, read only when the project is trusted |

Values are merged in this order, later layers winning: built-in defaults, user settings, project settings. Objects merge key by key. A scalar or a list in the project file replaces the user value. A list may also be patched with `+name` (add if absent) and `-name` (remove) entries. Environment variables override some settings; see [cli.md](cli.md#environment-variables).

Settings can be nested or dotted. These two files are the same:

```json
{ "compaction": { "reserveTokens": 16384 }, "oxa": { "mode": "plan" } }
```

```json
{ "compaction.reserveTokens": 16384, "oxa.mode": "plan" }
```

## Validation

A setting that is not in the list below is kept in the file, ignored, and reported as a warning at start-up: `unknown setting ...`. A setting with the wrong type is also reported, and the default is used. A file that is not valid JSON, or whose top level is not an object, is ignored with a warning (`settings file is not valid JSON ...; ignored`). Nothing in a settings file stops oxa from starting.

Settings that older files may contain but oxa does not implement are not listed here. Their use is reported as a warning: `setting ... is accepted but ignored in oxa`.

Where the user file alone counts, the table says so:

- user settings only: `defaultProjectTrust`, `httpProxy`, `oxa.allowHomeCwd`, `oxa.offline`, `oxa.engine.allowRemote`, and every `oxa.sandbox.*` key. A project file cannot set them, because a project file cannot widen what the model can reach.
- project settings are trust-gated. In an untrusted project they are not read. Project files that run or load code (`hooks`, `shellPath`, `shellCommandPrefix`, `packages`, `extensions`, `skills`, `prompts`) also need the content approval described in [security.md](security.md#project-trust).

Interactive selectors (`/settings`, `/thinking` and `/model` with Ctrl+S) write the user file.

## Keys

Types: `bool`, `number` (whole, not negative), `string`, `list` (of strings), `object`, and enumerations written as `a / b / c`. `any` means the value is not checked.

### Interface

| Key | Type | Default | Meaning |
|---|---|---|---|
| `$schema` | string | `null` | editor schema hint; ignored |
| `theme` | string | `"system"` | `system`, `dark`, `light` or a name from a theme file; also `oxa` (OXA's own dark palette) and `oxa-omarchy` (follows the current Omarchy desktop theme, read-only; `system` when there is none). With the `oxa` skin the default is `oxa-omarchy` |
| `ui.reducedMotion` | bool | `false` | no animation in the `oxa` skin: the working sprite and the YOLO flames are replaced by the static text badge. The environment variable `OXA_REDUCED_MOTION=1` does the same for one run; `NO_COLOR` and `TERM=dumb` imply it |
| `ui.skin` | `auto` / `pi` / `oxa` | `auto` | the look of the interactive UI: `pi` is pi's look, `oxa` is OXA's own (frames, a status line with meters, a key hint bar); `auto` uses oxa’s default appearance. With the `oxa` skin, `NO_COLOR` turns all colour off |
| `themes` | list | `[]` | extra theme files or directories |
| `tuiMode` | `fullscreen` / `regular` | `fullscreen` | start-up layout |
| `quietStartup` | `bool` or `"header"` | `false` | shorter start-up listing; `"header"` shows only the header |
| `hideThinkingBlock` | bool | `false` | hide thinking blocks in the transcript |
| `outputPad` | number | `1` | padding of output blocks, `0` or `1` |
| `editorPaddingX` | number | `0` | horizontal editor padding, 0 to 3 |
| `autocompleteMaxVisible` | number | `5` | rows of the autocomplete list |
| `markdown.codeBlockIndent` | string | `"  "` | prefix of each line of a code block |
| `markdown.mermaid` | `off` / `final` / `streaming` | `streaming` | accepted; diagrams are shown as code, so it has no effect |
| `fullscreenScrollbar` | `auto` / `always` / `hidden` | `auto` | scrollbar in full-screen mode; `auto` shows it while scrolling |
| `fullscreenWheelScrollLines` | number or `"auto"` | `"auto"` | lines per wheel step; `auto` speeds up fast spins |
| `fullscreenCopyOnSelect` | bool | `true` | copy a mouse selection on release |
| `fullscreenExitOutput` | `transcript` / `resume-hint` | `transcript` | what stays on screen after leaving full screen |
| `terminal.showImages` | bool | `true` | draw images in the terminal where the terminal supports it |
| `terminal.imageWidthCells` | number | `60` | width of drawn images, in cells |
| `terminal.clearOnShrink` | bool | `false` | clear empty rows when the output shrinks |
| `images.blockImages` | bool | `false` | do not send images to the model |
| `treeFilterMode` | `default` / `no-tools` / `user-only` / `labeled-only` / `all` | `default` | filter the tree opens with |
| `doubleEscapeAction` | `tree` / `fork` / `none` | `tree` | action of a double Escape on an empty editor |
| `showCacheMissNotices` | bool | `false` | show a notice for each cache miss ([compaction.md](compaction.md#cache-misses)) |
| `enableSkillCommands` | bool | `true` | list skills in the slash-command autocomplete |
| `followUpMode` | `all` / `one-at-a-time` | `one-at-a-time` | how queued follow-up messages are sent |
| `steeringMode` | `all` / `one-at-a-time` | `one-at-a-time` | how queued steering messages are sent |

### Models and thinking

| Key | Type | Default | Meaning |
|---|---|---|---|
| `defaultModel` | string | `null` | model to start with, when the engine serves it |
| `enabledModels` | list | `null` | model patterns for the Ctrl+P cycle and the start model of a new session |
| `defaultThinkingLevel` | `off` / `minimal` / `low` / `medium` / `high` / `xhigh` / `max` | `medium` | thinking level of a new session |
| `modelThinkingLevels` | object | `{}` | thinking level per model id, for example `{"your-model-id": "low"}` |
| `oxa.thinking.policy` | `fixed` / `auto` | unset | when thinking is on; unset uses `auto` ([profile.md](profile.md)) |
| `oxa.profile` | compatibility string | `local` | legacy compatibility setting pending removal; new setups use the defaults ([tools and context handling](profile.md)) |
| `cacheWarming` | `off` / `streaming` / `idle` | `idle` | `off` disables warm-up; `streaming` is accepted and behaves as `idle` |
| `oxa.bootPreload` | bool | `true` | prefill the frozen prefix when oxa starts, if the engine supports it |
| `oxa.prefill.paste` | bool | unset | while you type a message of at least 2000 bytes and stop for 0.7 seconds, with oxa idle, the engine prefills it so Enter starts sooner; only sent to an engine on this machine, and it needs an engine that supports prefill-only requests. Unset is off under `pi` and on under `local` |
| `oxa.prefill.tools` | bool | unset | while a tool call that runs alone takes more than 2 seconds, the engine prefills the results of the calls before it, once they total at least 2048 bytes; needs an engine that supports prefill-only requests. Unset is off under `pi` and on under `local` |

### Tools

| Key | Type | Default | Meaning |
|---|---|---|---|
| `defaultTools` | list | `["read","bash","edit","write","grep","find","ls"]` | tools on by default; names may include `subagent`, `web_search`, `codemode` and `tool_search` |
| `oxa.bash.defaultTimeout` | number (seconds) | unset | timeout of a bash call that names none; unset is no timeout under `pi` and 120 under `local`; `0` is no timeout |
| `oxa.diet.output` | bool | unset | the system prompt's "be concise" rule is replaced by rules on tool use and a final answer of at most 6 lines. Unset is off under `pi` and on under `local` |
| `oxa.diet.tools` | bool | unset | shorter `grep` and `bash` output: `grep` prints each path once, and runs of three or more blank lines in bash output become two. Unset is off under `pi` and on under `local` |
| `oxa.toolOutputCap` | number (bytes) | `16384` | cap on one tool output sent to the model. When `oxa.compaction.softTokens` is set or the engine publishes a compaction point, one tool result is at most a quarter of it (never below 2048 bytes), unless this is set; `grep`, `find`, `ls` and a `read` with `offset` or `limit` are cut at that cap, the first page of `read` at 8 KiB within it; see [profile.md](profile.md) |
| `oxa.toolRepair` | bool | `true` | repair a malformed tool call with one extra turn ([tools.md](tools.md#repair-turn)) |
| `oxa.subagent.enabled` | bool | unset | the `subagent` tool; unset is on unless the model is Flash or its window is below 32K |
| `oxa.subagent.smallContext` | bool | `false` | declare `subagent` on a model with a window below 32K |
| `oxa.subagent.softTokens` | number | `24000` | compaction point of a child agent |
| `oxa.subagent.resultCap` | number (bytes) | `6144` | cap on the text a child returns |
| `oxa.webSearch.enabled` | bool | `true` | the `web_search` tool, when a key exists |
| `oxa.webSearch.provider` | `brave` / `none` | `brave` | search provider; `none` turns the tool off |
| `oxa.webSearch.maxTokens` | number | `2048` | size of the search result the provider returns |
| `codemode.enabled` | bool | `false` | the `codemode` tool ([extensions.md](extensions.md#codemode-and-tool_search)) |
| `codemode.mode` | `on` / `only` | `on` | `only` hides every other tool while codemode is on |
| `codemode.toolSearch` | bool | `false` | the `tool_search` and `call_tool` tools |
| `codemode.maxWorkers` | number | `0` | script workers alive at once; `0` is 2 |
| `shellPath` | string | `null` | shell for bash and `!cmd`; it must be visible in the sandbox |
| `shellCommandPrefix` | string | `null` | text placed before each bash command, on its own line |
| `httpProxy` | string | `null` | proxy for MCP HTTP requests; user settings only |
| `httpIdleTimeoutMs` | number or `"disabled"` | `300000` | silence limit on the engine stream; `0` or `"disabled"` means none |
| `hooks` | object | `{}` | exec hooks, by event name ([extensions.md](extensions.md#exec-hooks)) |
| `extensions` | list | `[]` | extension files and directories to load |
| `skills` | list | `[]` | skill files and directories to load |
| `prompts` | list | `[]` | prompt template files and directories to load |
| `packages` | list | `[]` | installed package sources; `oxa install` edits this |

### Sessions and compaction

| Key | Type | Default | Meaning |
|---|---|---|---|
| `sessionDir` | string | `null` | session storage; `--session-dir` and `OXA_SESSION_DIR` win over it |
| `compaction.enabled` | bool | `true` | compact automatically; `false` keeps only the emergency compaction |
| `compaction.reserveTokens` | number | `16384` | room kept free for the summary and the next reply |
| `compaction.keepRecentTokens` | number | `4000` | recent context kept verbatim after a compaction |
| `compaction.modelOverrides` | object | `null` | per-model `reserveTokens` and `keepRecentTokens` |
| `oxa.compaction.idleSummary` | bool | unset | after a run that leaves the context between 80 % and 100 % of the soft limit, the summary of the next compaction is written in idle time, and that compaction reuses it with no model request if nothing it covers has changed. Unset is off under `pi` and on under `local` |
| `oxa.compaction.midRun` | bool | unset | compact between tool steps of a run when the context passes the compaction point; unset is off under `pi` and on under `local` |
| `oxa.compaction.summaryTarget` | number (tokens) | unset | soft length target of a compaction summary: the model is asked for about three quarters of it in words; `0` is no target. Unset is no target under `pi` and 700 under `local` |
| `oxa.compaction.softTokens` | number | `null` | context size that starts an idle compaction; unset uses the engine's point |
| `oxa.compaction.turnStart` | bool | unset | compact before a prompt is sent when the context is already above the compaction point (for example after an aborted turn); unset is off under `pi` and on under `local` |
| `branchSummary.reserveTokens` | number | `16384` | output budget of a branch summary |
| `branchSummary.skipPrompt` | bool | `false` | leave out the "summarise the branch?" question |

The compaction rules are in [compaction.md](compaction.md).

### Retries

| Key | Type | Default | Meaning |
|---|---|---|---|
| `retry.enabled` | bool | `true` | retry a failed request in the agent loop |
| `retry.maxRetries` | number | `3` | retries of an agent request |
| `retry.baseDelayMs` | number | `2000` | first delay, doubled each retry |
| `retry.maxAgentDelayMs` | number | `60000` | longest delay the agent loop waits |
| `retry.provider.maxRetries` | number | `0` | silent retries of one HTTP request, before the first byte of the reply |
| `retry.provider.maxRetryDelayMs` | number | `60000` | longest wait of one HTTP retry |
| `retry.provider.timeoutMs` | number | `null` | silence limit of the engine stream; unset uses `httpIdleTimeoutMs` |

### Permissions, trust and offline mode

| Key | Type | Default | Meaning |
|---|---|---|---|
| `oxa.allowHomeCwd` | bool | `false` | allow oxa to start in the home directory, `/` or a parent of home. Without it print, JSON and RPC modes refuse and interactive mode asks; the environment variable `OXA_ALLOW_HOME_CWD` also allows it. User settings only. See [security.md](security.md#starting-in-your-home-directory) |
| `oxa.mode` | `plan` / `workspace` / `ask` | `workspace` | permission mode at start |
| `defaultProjectTrust` | `ask` / `always` / `never` | `ask` | what happens in a project that is not trusted yet; user settings only |
| `oxa.offline` | bool | `false` | same as `--offline`; user settings only |
| `oxa.engine.allowRemote` | bool | `false` | allow an engine on another host; user settings only |
| `oxa.sandbox.net` | `on` / `off` | `on` | network for bash in workspace and ask modes |
| `oxa.sandbox.allowHomeWrite` | bool | `false` | let bash write to the working directory when oxa starts in the home directory or above it (a start in `/` makes the whole system writable); user settings only |
| `oxa.sandbox.readPaths` | list | `[]` | extra read-only paths for bash; `~/` is expanded; user settings only |
| `oxa.sandbox.writePaths` | list | `[]` | extra writable paths for bash; `~/` is expanded; user settings only |

### Other

| Key | Type | Default | Meaning |
|---|---|---|---|
| `externalEditor` | string | `null` | editor for Ctrl+G; wins over `VISUAL` and `EDITOR` |
| `oxa.reportIssue.repo` | string | `null` | repository for `oxa report-issue submit`, as `OWNER/NAME` |
| `oxa.builtinSkills` | bool | `true` | include the skills built into oxa |

## Example

A user file that sets a default model, a thinking level, a tool list and a subagent switch:

```json
{
  "defaultModel": "your-model-id",
  "defaultThinkingLevel": "low",
  "defaultTools": ["read", "grep", "find", "ls", "edit", "write"],
  "compaction": { "reserveTokens": 16384, "keepRecentTokens": 4000 },
  "oxa": { "mode": "workspace", "subagent": { "enabled": false } }
}
```

A project file that sets a retry count and an exec hook for one repository:

```json
{
  "retry": { "maxRetries": 5 },
  "hooks": { "tool_call": "/usr/local/bin/log-tool-call" }
}
```

The project's `hooks` entry runs a program. oxa loads it only after you approve the project's content, and asks again when that content changes.
