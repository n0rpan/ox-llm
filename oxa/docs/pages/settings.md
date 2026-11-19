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

- user settings only: `defaultProjectTrust`, `httpProxy`, `oxa.allowHomeCwd`, `oxa.offline`, `oxa.engine.allowRemote`, `oxa.engine.inProcessCloud`, `oxa.cloud`, `oxa.engine.inProcessLocal`, `oxa.local`, `oxa.lead.shadow`, `oxa.lead.serve`, `oxa.lead.sessionId`, `oxa.lead.model`, and every `oxa.sandbox.*` key. A project file cannot set them, because a project file cannot widen what the model can reach.
- project settings are trust-gated. In an untrusted project they are not read. Project files that run or load code (`hooks`, `shellPath`, `shellCommandPrefix`, `packages`, `extensions`, `skills`, `prompts`) also need the content approval described in [security.md](security.md#project-trust).

Interactive selectors (`/settings`, `/thinking` and `/model` with Ctrl+S) write the user file.

## Keys

Types: `bool`, `number` (whole, not negative), `string`, `list` (of strings), `object`, and enumerations written as `a / b / c`. `any` means the value is not checked.

### Interface

| Key | Type | Default | Meaning |
|---|---|---|---|
| `oxa.lead.serve` | object | `{}` | User-only, default-off [lead web access](lead-web.md); local mode unless selected otherwise. |
| `$schema` | string | `null` | editor schema hint; ignored |
| `theme` | string | `"system"` | `system`, `dark`, `light` or a name from a theme file; also `oxa` (OXA's own dark palette) and `oxa-omarchy` (follows the current Omarchy desktop theme, read-only; `system` when there is none). With the `oxa` skin the default is `oxa-omarchy` |
| `ui.animation` | `arcade` / `minimal` / `off` | `arcade` | what moves on the activity line of the `oxa` skin while a run is in progress: `arcade` is a one-row animated scene with a short `CLEAR` when the run is done; `minimal` is the small six-cell sprite; `off` is the static text badge. Never any sound. `ui.reducedMotion`, `NO_COLOR` and `TERM=dumb` give `off`. `--animation <mode>` overrides this key for one run |
| `ui.animationGraphics` | `auto` / `off` | `auto` | `auto` draws the scene as pixel art when the terminal answers the kitty graphics query at start and is kitty or Ghostty (every frame is sent once and deleted again on exit); `off` keeps the scene in text everywhere |
| `ui.animationScene` | `graphics` / `stack` / `stars` / `charge` | `null` | the scene of `ui.animation = "arcade"`. Unset: the pixel-art scene where the terminal can draw it, else `stack` (stacking columns). `stack`, `stars` (a ship in a starfield) and `charge` (the ox, in block characters) are the text scenes; `graphics` asks for the pixel-art scene on any terminal that answers the graphics query |
| `ui.notify` | `off` / `unfocused` / `always` | `unfocused` | terminal notification when a run settles (including errors/aborts) or a permission dialog opens. `unfocused` requires a terminal focus-out report; terminals without focus reporting stay quiet. Uses OSC 9 and a BEL fallback from oxa itself, with generic text (no prompt or file details); any sound is controlled by the terminal. `off` disables both |
| `ui.reducedMotion` | bool | `false` | no animation in the `oxa` skin: the working sprite and the YOLO flames are replaced by the static text badge. The environment variable `OXA_REDUCED_MOTION=1` (also `true`, `yes` or `on`) does the same for one run, and `OXA_REDUCED_MOTION=0` (also `false`, `no` or `off`) turns it off for one run; `NO_COLOR` and `TERM=dumb` imply it |
| `ui.skin` | `auto` / `pi` / `oxa` / `ascii` | `auto` | the look of the interactive UI: `oxa` is OXA's own (frames, a status line with meters, a key hint bar), `pi` is the plain look, `ascii` is `oxa` with ASCII glyphs only, and `auto` is `oxa` (ASCII when the locale is not UTF-8 or `TERM` is `linux`/`dumb`). With the `oxa` skin, `NO_COLOR` turns all colour off |
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
| `oxa.thinking.budget` | non-negative integer / null | unset | optional reasoning-text token cap, only while thinking is on and a local engine advertises `thinking_budget`; `0` closes reasoning immediately. Unknown and remote engines omit this local field. The total output limit still applies; no default changes ([thinking policy](tools-context.md)) |
| `oxa.thinking.policy` | `fixed` / `auto` | unset | when thinking is on; unset is `auto` ([tools-context.md](tools-context.md)) |
| `oxa.thinking.toolFollowups` | `auto` / `on` / `off` | `auto` | thinking on the replies after tool results, when `oxa.thinking.policy` is `auto`. `off`: never. `on`: always. `auto`: off for routine steps, on while the run needs reasoning: after a failed call (non-zero exit, an edit that did not apply, a failing test or build), after a repeat notice, or after `oxa.thinking.escalation.fruitlessCalls` calls without progress. It goes back off after a successful edit or write or a passing test or build ([tools.md](tools.md#thinking-on-tool-follow-ups)). The level `off` always wins |
| `oxa.thinking.escalation.fruitlessCalls` | number | `6` | under `oxa.thinking.toolFollowups: auto`, the calls without an edit or write since the prompt that turn thinking on (re-reads, repeated calls, commands; reading a new file or range is progress and not counted); `0` = never on this ground |
| `cacheWarming` | `off` / `streaming` / `idle` | `idle` | `off` disables warm-up; `streaming` is accepted and behaves as `idle` |
| `oxa.bootPreload` | bool | `true` | prefill the frozen prefix when oxa starts, if the engine supports it |
| `oxa.prefill.paste` | bool | unset | while you type a message of at least 2000 bytes and stop for 0.7 seconds, with oxa idle, the engine prefills it so Enter starts sooner; only sent to an engine on this machine, and it needs an engine that supports prefill-only requests. Unset is on |
| `oxa.prefill.tools` | bool | unset | while a tool call that runs alone takes more than 2 seconds, the engine prefills the results of the calls before it, once they total at least 2048 bytes; needs an engine that supports prefill-only requests. Unset is on |
| `oxa.sampling.mode` | `greedy` \| `model` | `greedy` | `greedy` sends `temperature: 0` (deterministic; the engine's speculative decoding needs it). `model` sends the sampling the model card recommends (Qwen3.6 and Ornith: temperature 0.6, top_p 0.95, top_k 20 with thinking on; 0.7, 0.8, 20 and presence_penalty 1.5 with thinking off) for chat and subagent replies; other models stay greedy |
| `oxa.sampling.seed` | number | unset | a fixed seed sent with `oxa.sampling.mode: model` |

### Tools

| Key | Type | Default | Meaning |
|---|---|---|---|
| `defaultTools` | list | `["read","bash","edit","write","grep","find","ls"]` | tools on by default; names may include `subagent`, `web_search`, `codemode` and `tool_search` |
| `oxa.bash.defaultTimeout` | number (seconds) | unset | timeout of a bash call that names none; unset is 120; `0` is no timeout |
| `oxa.diet.output` | bool | unset | the system prompt's "be concise" rule is replaced by rules on tool use and a final answer of at most 6 lines. Unset is on |
| `oxa.diet.tools` | bool | unset | shorter `grep` and `bash` output: `grep` prints each path once, and runs of three or more blank lines in bash output become two. Unset is on |
| `oxa.maxTurns` | number | unset | end a run after this many model replies with a `turn_limit` error (a cap against loops the repeat guard cannot see, such as alternating calls). Unset or `0` is no limit |
| `oxa.toolOutputCap` | number (bytes) | `16384` | cap on one tool output sent to the model. When `oxa.compaction.softTokens` is set or the engine publishes a compaction point, one tool result is at most a quarter of it (never below 2048 bytes), unless this is set; `grep`, `find`, `ls` and a `read` with `offset` or `limit` are cut at that cap, the first page of `read` at 8 KiB within it; see [tools-context.md](tools-context.md) |
| `oxa.toolRepair` | bool | `true` | repair a malformed tool call with one extra turn ([tools.md](tools.md#repair-turn)) |
| `oxa.toolRepeatGuard` | bool | `true` | answer a tool call that returned the same result three times in a row with a short note, and stop the run if it goes on ([tools.md](tools.md#repeated-calls)) |
| `oxa.subagent.enabled` | bool | unset | the `subagent` tool; unset is on unless the model is Flash or its window is below 32K |
| `oxa.subagent.smallContext` | bool | `false` | declare `subagent` on a model with a window below 32K |
| `oxa.subagent.softTokens` | number | `24000` | compaction point of a child agent |
| `oxa.subagent.remoteConcurrency` | number | `4` | children of one `tasks` call that run at once when some run on a cloud model (local children run one at a time) |
| `oxa.subagent.resultCap` | number (bytes) | `6144` | cap on the text a child returns |
| `oxa.webSearch.enabled` | bool | `true` | the `web_search` tool, when a key exists |
| `oxa.webSearch.provider` | `brave` / `none` | `brave` | search provider; `none` turns the tool off |
| `oxa.webSearch.maxTokens` | number | `2048` | size of the search result the provider returns |
| `codemode.enabled` | bool | `false` | the `codemode` tool ([extensions.md](extensions.md#codemode-and-tool_search)) |
| `codemode.mode` | `on` / `only` | `on` | `only` hides every other tool while codemode is on |
| `codemode.toolSearch` | bool | `false` | the `tool_search` and `call_tool` tools |
| `codemode.maxWorkers` | number | `0` | script workers alive at once; `0` is 2 |
| `done` | string list | `[]` | shell commands oxa runs itself when a run that edited files (`edit`/`write` tools) is about to finish, in the project folder, in the sandbox (10 min limit, 4 KB of output each). An entry of the form `in <dir>: <command>` runs in `<dir>` instead, for example `in oxa: cargo fmt --check` when the Rust workspace is in the `oxa/` subfolder; `<dir>` is relative to the project folder (an absolute path inside it works too). A folder that does not exist, or that leaves the project folder (through `..`, an absolute path or a symlink), is not run and the check shows `FAILED (not run)` with the reason; the notice shows the folder as `[oxa] cargo fmt --check`. A failure is sent back to the model as a message so it can fix it; after 2 automatic rounds the run stops and the failure stays in the transcript; in `-p` and `--mode json` oxa then prints `done checks failed` on standard error and exits 1 (the same code as any other failed run; the interactive mode only shows the failure). A project's `done` in an untrusted project is not run: `-p` prints `done checks in .oxa/settings.json were not run: project is not trusted` on standard error (interactive mode shows it as a startup notice). Not re-run unless files were edited again. In a git repository, changes through `bash` are detected from the index, file list and file metadata without reading file contents. On filesystems with coarse change timestamps, a same-size in-place rewrite that restores mtime within the same ctime tick can be missed; successful `edit`/`write` calls always count as edits. Project value wins and needs project trust |
| `shellPath` | string | `null` | shell for bash and `!cmd`; it must be visible in the sandbox |
| `shellCommandPrefix` | string | `null` | text placed before each bash command, on its own line |
| `httpProxy` | string | `null` | proxy for MCP HTTP requests; user settings only |
| `httpIdleTimeoutMs` | number or `"disabled"` | `300000` | silence limit on the engine stream; `0` or `"disabled"` means none |
| `hooks` | object | `{}` | exec hooks, by event name ([extensions.md](extensions.md#exec-hooks)) |
| `extensions` | list | `[]` | extension files and directories to load |
| `skills` | list | `[]` | skill files and directories to load |
| `prompts` | list | `[]` | prompt template files and directories to load |
| `packages` | list | `[]` | installed package sources; `oxa install` edits this |
| `permissions.defaultMode` | `standard` / `auto` / `yolo` / `plan` | `auto` | permission mode at start ([security.md](security.md#standard-auto-and-yolo)); command-line flags win for one run; a project may lower it, never raise it |

### Sessions and compaction

| Key | Type | Default | Meaning |
|---|---|---|---|
| `sessionDir` | string | `null` | session storage; `--session-dir` and `OXA_SESSION_DIR` win over it |
| `compaction.enabled` | bool | `true` | compact automatically; `false` keeps only the emergency compaction |
| `compaction.reserveTokens` | number | `16384` | room kept free for the summary and the next reply |
| `compaction.keepRecentTokens` | number | `4000` | recent context kept verbatim after a compaction |
| `compaction.modelOverrides` | object | `null` | per-model `reserveTokens` and `keepRecentTokens` |
| `compaction.reactivateSkills` | bool | `false` | retain invoked skills across compaction and ask the model to re-read them. User settings only |
| `oxa.compaction.idleSummary` | bool | unset | after a run that leaves the context between 80 % and 100 % of the soft limit, the summary of the next compaction is written in idle time, and that compaction reuses it with no model request if nothing it covers has changed. Unset is on |
| `oxa.compaction.midRun` | bool | unset | compact between tool steps of a run when the context passes the compaction point; unset is on |
| `oxa.compaction.summaryTarget` | number (tokens) | unset | soft length target of a compaction summary: the model is asked for about three quarters of it in words; `0` is no target. Unset is 700 |
| `oxa.compaction.softTokens` | number | `null` | context size that starts an idle compaction; unset uses the engine's point |
| `oxa.compaction.turnStart` | bool | unset | compact before a prompt is sent when the context is already above the compaction point (for example after an aborted turn); unset is on |
| `branchSummary.reserveTokens` | number | `16384` | output budget of a branch summary |
| `branchSummary.skipPrompt` | bool | `false` | leave out the "summarise the branch?" question |

The compaction rules are in [compaction.md](compaction.md).

### Retries

| Key | Type | Default | Meaning |
|---|---|---|---|
| `retry.enabled` | bool | `true` | retry a failed request; off sends each request once, with no provider retry |
| `retry.maxRetries` | number | `3` | retries of an agent request |
| `retry.baseDelayMs` | number | `2000` | first delay, doubled each retry |
| `retry.maxAgentDelayMs` | number | `60000` | longest delay the agent loop waits |
| `retry.provider.maxRetries` | number | `0` | silent retries of one HTTP request, before the first byte of the reply; `retry.enabled` false disables them |
| `retry.provider.maxRetryDelayMs` | number | `60000` | longest wait of one HTTP retry |
| `retry.provider.timeoutMs` | number | `null` | silence limit of the engine stream; unset uses `httpIdleTimeoutMs` |

### Permissions, trust and offline mode

| Key | Type | Default | Meaning |
|---|---|---|---|
| `oxa.allowHomeCwd` | bool | `false` | allow oxa to start in the home directory, `/` or a parent of home. Without it print, JSON and RPC modes refuse and interactive mode asks; the environment variable `OXA_ALLOW_HOME_CWD` also allows it. User settings only. See [security.md](security.md#starting-in-your-home-directory) |
| `oxa.mode` | `plan` / `workspace` / `ask` | `workspace` | table mode at start |
| `defaultProjectTrust` | `ask` / `always` / `never` | `ask` | what happens in a project that is not trusted yet; user settings only |
| `oxa.offline` | bool | `false` | same as `--offline`; user settings only |
| `oxa.engine.allowRemote` | bool | `false` | allow an engine on another host; user settings only |
| `oxa.engine.inProcessLocal` | bool | `false` | select the in-process owned local engine, including offline; user settings only |
| `oxa.local` | object | `{}` | literal native binary/model/arguments/environment and model limits; user settings only; see [native local configuration](../../oxc-local/CONFIGURATION.md) |
| `oxa.lead.shadow` | bool | `false` | enable the read-only `oxa lead --shadow` proposal preset; user settings only; no live runner actions |
| `oxa.engine.inProcessCloud` | bool | `false` | select the in-process Rust cloud gateway for sessions and model discovery; user settings only |
| `oxa.cloud` | object | `{}` | native cloud models, prices, USD budget and subscription caps; user settings only; see below |
| `oxa.lead.serve` | object | `{}` | the lead web access scaffold: `enabled`, `identity` (`local` or `tailscale`), `bind` (a loopback address), `port`, `allowed_identities`. Off by default and no listener is installed yet; user settings only. See [lead-web.md](lead-web.md) |
| `oxa.lead.sessionId` | string | unset | exact ID of the dedicated Lead Session created by the native owner operation, from owner-private user settings. Free text is disabled when unset. |
| `oxa.lead.model` | string | unset | exact model id the Lead conversation resolves through the normal `--model` selector and gateway; user settings only, read from the owner-private settings file. Unset keeps the ordinary startup model; a route that does not resolve is reported, never silently replaced. A metered model still needs its daily USD cap, so choose a subscription route while the cap is absent. |
| `oxa.envNote` | bool | unset | append the environment note to each prompt you send: the first one in a session (and the first of a new day, or after a compaction) gets a short `<env>` block with the date, time, time zone, locale, kernel, model, context size, thinking level and permission mode; later ones get `[local time HH:MM ZONE]`. It is never put in the system prompt, so the prompt cache stays exact. Unset is on; `OXA_ENV_NOTE=0` or `=1` overrides it for one run |
| `oxa.sandbox.net` | `on` / `off` | `on` | network for bash in workspace and ask modes |
| `oxa.sandbox.allowHomeWrite` | bool | `false` | make the home directory the bash workspace when oxa starts in it, in every mode (its folders and files writable; dotfiles, dot-folders, `~/bin` and the home folder itself stay read-only; never `/` or a parent of home). In interactive mode confirming the start question does the same. User settings only. See [security.md](security.md#starting-in-your-home-directory) |
| `oxa.sandbox.readPaths` | list | `[]` | extra read-only paths for bash; `~/` is expanded; user settings only |
| `oxa.sandbox.writePaths` | list | `[]` | extra writable paths for bash; `~/` is expanded; user settings only |

### Native cloud gateway

See [Run oxa without Python](no-python.md) for setup, remaining Python dependencies
and a loopback-only manual check.

Set `oxa.engine.inProcessCloud` in the user settings file to select the Rust
gateway for sessions and `--list-models`. The default is `false`. Offline mode
refuses before opening the gateway; configuration or gateway failures are returned
directly. Project settings cannot enable this transport or change its cloud policy.

Set `oxa.cloud.discover_models` to `true` to discover models at startup for the
configured OpenRouter, OpenAI and Anthropic API keys. It also enables the
OpenCode model cache when that provider is configured. A fresh cached catalog
avoids a discovery request. A stale OpenRouter catalog is refreshed at startup
before its prices can be used to reserve spend. Other stale catalogs can be
shown while a bounded refresh updates the cache for later starts. A failed live
lookup uses the configured catalog and reports the failure. Discovery stays
off in offline mode.

OpenRouter supplies model limits and token prices. For a discovered model with
no known rates, the owner can opt in by adding its exact public alias to
`oxa.cloud.unpriced_discovered_pricing` in user settings. Each entry needs
positive `input_per_million` and `output_per_million` USD rates. Choose rates
that bound the expected provider charge. The gateway uses them for the normal
budget reservation and records the settlement as `estimated_cost_usd`. Missing
final usage retains the reservation. Estimates count against the daily cap and
are recorded separately from actual costs. The setting is empty by default and
is only read when discovery is enabled. The model still needs known token limits.
Known provider or configured prices take precedence over this setting.
When the same provider has several configured base URLs, select the discovery
endpoint with `oxa.cloud.discovery.<provider>.base_url`.

The merged catalog is available through the model picker and `--list-models`.
The LAN guest catalog lists only guest-allowlisted models.

Configure `anthropic`, `openai` (Responses API) or `chatgpt` (subscription) routes
under `oxa.cloud.models`, keyed by the model name used
with `--model`. Supply the upstream model, context/output limits and current
provider prices. This example uses an illustrative model and prices:

```json
{
  "oxa": {
    "engine": { "inProcessCloud": true },
    "cloud": {
      "daily_budget_usd": 10,
      "models": {
        "example-cloud": {
          "provider": "anthropic",
          "upstream_model": "replace-with-your-model",
          "context_window": 128000,
          "max_output": 8192,
          "pricing": {
            "input_per_million": 2,
            "output_per_million": 3,
            "cache_read_per_million": 0.2,
            "cache_write_per_million": 2.5
          }
        }
      }
    }
  }
}
```

The key is read for each request from the existing private
`~/.config/oxc/llm/cloud-secrets.json` (owner-only, mode `0600`, outside a checkout).
Keep keys out of settings. The file uses the existing provider-key JSON format.
Private settlement rows live in `<agent directory>/cloud-ledger/`. Listing models
reads neither the key nor the ledger and contacts no provider. A missing price or
daily budget refuses a request before credentials or provider I/O. Requests retain
their budget reservation until the private row is durable.

An `openai` route uses the same model/price fields, reads the `openai` key from
that key file, and defaults to `https://api.openai.com/v1`. Both native providers
preserve tool argument strings, including arguments supplied only by final
provider events. Completed native replay is authenticated to the provider,
model, account and exact visible history. It is stored only in private session
records, omitted from public output, and dropped when compaction changes context.
An invalid saved binding refuses the request before provider I/O.

A `chatgpt` route defaults to `https://chatgpt.com/backend-api/codex` and uses the
existing owner-only `chatgpt-auth.json` beside the key file. It does not discover
other applications' credentials or start a sign-in. Configure the user-only
`oxa.cloud.subscription_limits` object with positive integer `daily_requests`
and `daily_tokens`. The separate optional `oxa.cloud.subscription` refresh object
accepts a public `client_id` override (`[A-Za-z0-9_.-]{1,128}`); its default is the
experimental public identity approved in #252. The refresh endpoint is fixed at
`https://auth.openai.com/oauth/token` and cannot be overridden in settings.
`refresh_timeout_ms` defaults to 15000, must be positive and is bounded by the
client's 30-second maximum. These settings contain no access or refresh token.
Client transport tests inject loopback endpoints privately; production refresh
configuration never supplies one.

Subscription calls require their own positive request/token caps, retain actual
token usage and settle zero USD. Unknown paid work retains its full token bound;
the cap includes concurrent and crashed-process reservations. It never falls back
to a paid API balance. Missing metered pricing still refuses metered requests.

### Other

| Key | Type | Default | Meaning |
|---|---|---|---|
| `externalEditor` | string | `null` | editor for Ctrl+G; wins over `VISUAL` and `EDITOR` |
| `oxa.reportIssue.repo` | string | `null` | repository for `oxa report-issue submit`, as `OWNER/NAME` |
| `oxa.reportIssue.semanticDedupe` | bool | `false` | verify dogfood reports against current main and search open and recently closed issues before posting. Matching reports become comments. New issues are limited to one per task and three per worker per UTC day |
| `oxa.builtinSkills` | bool | `true` | include the skills built into oxa |
| `oxa.share.remoteModels` | string list | `[]` | user-only allowlist of metered cloud aliases eligible for explicit LAN sharing; personal subscriptions are excluded |
| `oxa.skills.includeSynced` | bool | `false` | also list the skills another product synced into `~/.agents/skills` (a `synced/` folder, or a folder whose `manifest.json` lists `skillId`s); they are written for that product's tools, so oxa skips them by default |

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
