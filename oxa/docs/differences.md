# Differences from pi 1.5.2

oxa is a small local agent tool derived from the pi coding agent. This page lists where
oxa differs: the rows that are adapted for the local engine, and the pi features that oxa
does not have. Rows that behave as pi documents are not listed. The table is generated
from `PARITY.md` by `ci/gen_docs.py`; do not edit it by hand.

Each row has an id in the first column, of the form `AREA-NNN`. The id is the row's key in oxa's internal parity
table, which lists each documented behaviour of the earlier agent with its verdict and the
test that checks it. The ids are shown so that a row can be referred to; the text of the
rows does not cite the internal work items.

## CLI: Command line

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| CLI-003 | `@path` with an image attaches the image | text-only engine: stored + shown, wire gets the fixed placeholder line |
| CLI-012 | `--model <pattern>`: exact id, fuzzy match, optional `:<thinking>` suffix | resolved over the engine model list (`/v1/models`); `provider/id` accepted only for a configured engine endpoint; a switch is a new epoch |
| CLI-013 | `--api-key <key>` | optional bearer key for an engine endpoint; no provider selection |
| CLI-014 | `--thinking <level>`: off..max, overrides the model suffix, clamped | off = `enable_thinking:false`, any other level = true |
| CLI-015 | `--models <patterns>`: comma-separated scope (exact, fuzzy, glob, `:<thinking>`) | over the engine model list |
| CLI-016 | `--list-models [search]`: list and exit | engine models only |
| CLI-028 | `--tools` keeps MCP tools unless an entry starts with `mcp__`; unmatched MCP tools are never declared directly | direct MCP tools are frozen at epoch start |
| CLI-032 | Default enabled tools are `read`, `bash`, `edit`, `write` | all 7 built-ins on by default; `subagent` is on too where the model allows it (not on the Rust Flash engine, not below a 32K window; `oxa.subagent.enabled` wins); `defaultTools` restores pi's four |
| CLI-034 | Built-in extensions `codemode` and `tool_search` are off by default; MCP turns them on when needed | `tool_search` returns matches as text; calls go through the frozen `call_tool` dispatcher |
| CLI-035 | `-e`, `--extension <path>`: file, directory or `builtin:<name>`; repeatable | JavaScript in QuickJS; a `.ts` file is always rejected, with a hint to write JavaScript |
| CLI-047 | `--offline` | hard switch: every connection of the harness to a host that is not loopback is refused with one error, the bash sandbox has no network, oxa's own git and `gh` calls refuse, `web_search` is not declared; the local engine keeps working. Keeps pi's meaning (no automatic network activity; oxa has none) |
| CLI-051 | `oxa install <source>` / `oxa remove <source>` (`uninstall` alias) / `oxa list` | `oxa install\|remove\|list`; git and local sources; `npm:` is refused |
| CLI-055 | `oxa update --extensions` / `oxa update <source>` / `--extension <source>` | `oxa update` for git + local packages |
| CLI-064 | No engine reachable: one-line error + hint, exit code 2 | oxa only: `engine_down` |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| CLI-011 | `--provider <name>` | one provider type (the engine); a non-engine provider is a clear error |
| CLI-033 | Built-in tool `powershell` | Linux only |
| CLI-053 | `oxa config --import-pi` | omitted (removed with pi import) |
| CLI-054 | `oxa update` (self), `--self`, `self`, `pi`, `--all`, `OXA_SELF_UPDATE_COMMAND`, `selfUpdateCommand` | no self-update, no version check (no network unless asked) |
| CLI-056 | `oxa update --models` | no model catalogs |
| CLI-057 | `oxa auth check\|print-api-key\|print-bearer-token` and their options | no provider credentials, no OAuth |
| CLI-058 | `oxa login` options `--api-key`, `--no-browser` | no provider sign-in |

## MOD: Models, providers, thinking

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| MOD-001 | Model selection: `/model`, Ctrl+L selector, Ctrl+P / Shift+Ctrl+P cycling over the scoped list | over the engine model list; a switch is a new epoch and prints its re-prefill cost |
| MOD-002 | `models.json` custom models | names engine endpoints (laptop, LAN) with an optional bearer key; other provider types are a clear error |
| MOD-003 | Thinking levels off..max with per-model clamping | boolean on the wire; `thinkingBudgets` ignored with a warning |
| MOD-004 | Model without preserved thinking: thinking defaults to off, status line says why | (oxa only) |
| MOD-010 | llama.cpp router management (`/llama`) | `/engine`: status and model load over `/_local/status` |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| MOD-005 | Providers: 20+ cloud providers, API keys, `auth.json` | one provider type: the oxe engine |
| MOD-006 | Subscription / OAuth sign-in (`/login`, `/logout`) | no accounts |
| MOD-007 | Custom providers (`registerProvider`, custom-provider.md) |  |
| MOD-008 | Virtual models (virtual-models.md) |  |
| MOD-009 | Classifier and image models (`models.classify`, `models.generateImage`) | no such engine endpoints; calls return `unsupported_in_oxa` |
| MOD-011 | Remote model catalog refresh | no network unless asked |

## SET: Settings

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| SET-004 | `defaultModel` | an engine model id |
| SET-005 | `defaultThinkingLevel` (default `medium`) | boolean |
| SET-006 | `modelThinkingLevels` | keyed by engine model id |
| SET-008 | `enabledModels` | patterns over engine models |
| SET-010 | `showCacheMissNotices` | uses exact `cached_tokens`; cause from `miss_cause` |
| SET-011 | `cacheWarming` `off\|streaming\|idle` | maps to idle prefill (`max_tokens:0` with the snapshot and park requests); `streaming` = `idle` |
| SET-018 | `defaultTools`: plain names replace, `+name` adds, `-name` removes | default set is the 7 built-ins + `subagent` where offers it; `+name`/`-name` edit that set (incl. `subagent`); plain names replace it; resolved per epoch |
| SET-021 | `sessionDir` | the session store keeps one sub-directory per working directory below every sessions directory (pi stores flat in a custom `sessionDir`) |
| SET-024 | `compaction.reserveTokens` (16384) | hard limit = `n_ctx - max_tokens - reserveTokens` |
| SET-025 | `compaction.keepRecentTokens` | default 4000 instead of 20000: the kept tail is re-prefilled |
| SET-026 | `compaction.modelOverrides` | keyed by engine model id; parsed, applied only when it names the session model |
| SET-027 | `oxa.compaction.softTokens` | oxa only: soft limit that triggers compaction after the reply |
| SET-028 | `oxa.toolOutputCap` | oxa only: cap, default 16 KiB |
| SET-046 | `images.blockImages` | always effectively true on the wire; the setting only hides the inline display notice |
| SET-048 | `markdown.mermaid` `off\|final\|streaming` | Mermaid shown as a highlighted code block; setting accepted |
| SET-050 | `httpProxy` | applies to MCP HTTP only; the engine connection is direct |
| SET-051 | `httpIdleTimeoutMs` | silence limit of the engine stream; SSE comments count as liveness; no wall-clock limit |
| SET-053 | `retry.enabled`, `retry.maxRetries`, `retry.baseDelayMs`, `retry.maxAgentDelayMs` | replay only before the first data byte until `Idempotency-Key` exists |
| SET-054 | `retry.provider.timeoutMs`, `retry.provider.maxRetries`, `retry.provider.maxRetryDelayMs` | same rule; one HTTP layer |
| SET-061 | `hooks: {event: command}` | oxa only: exec hooks (JSON on stdin, reply on stdout, run in bwrap) |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| SET-003 | `defaultProvider` | accepted and ignored with a warning (one provider) |
| SET-007 | `thinkingBudgets` | ignored with a warning |
| SET-020 | `codemode.inlineBudget` (3000) | not in oxa: the codemode description is one fixed text, so the frozen prefix does not grow or change with the tool catalogue; tools are found with `searchTools` / `describeTool`; the key is an unknown setting |
| SET-022 | `sessionShareDestination` | `/share` omitted |
| SET-040 | `showHardwareCursor` | pi-specific, not in oxa: the terminal's own cursor is always shown at the editor's cursor (works with input methods); the key is an unknown setting |
| SET-043 | `terminal.showTerminalProgress` (OSC 9;4) | pi-specific, not in oxa: no OSC 9;4 progress; the key is an unknown setting |
| SET-044 | `terminal.hyperlinks`, `terminal.images`, `terminal.trueColor` overrides | pi-specific, not in oxa: terminal capabilities come from detection only; the keys are unknown settings |
| SET-045 | `images.autoResize` | images are never sent; accepted and ignored |
| SET-049 | `transport` | SSE only; other values warn |
| SET-052 | `websocketConnectTimeoutMs` | no websocket |
| SET-057 | `npmCommand` | pi-specific, not in oxa: no npm package sources; the key is an unknown setting (warned, ignored) |
| SET-058 | `selfUpdateCommand` | no self-update |
| SET-062 | `collapseChangelog` | pi-specific, not in oxa: no changelog display; the key is an unknown setting |
| SET-063 | `enableInstallTelemetry`, `enableAnalytics` | no telemetry; accepted and ignored |
| SET-064 | `warnings.anthropicExtraUsage` | no cloud providers |

## KEY: Keybindings

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| KEY-011 | `app.clipboard.pasteImage` (ctrl+v) | pastes files and images into the session; an image is shown, the wire gets the placeholder |
| KEY-013 | Models: `app.model.select` (ctrl+l), `cycleForward` (ctrl+p), `cycleBackward` (shift+ctrl+p), `app.models.save` (ctrl+s) | over engine models; a switch is a new epoch |
| KEY-014 | Thinking: `app.thinking.cycle` (shift+tab), `app.thinking.save` (ctrl+s), `app.thinking.toggle` (ctrl+t) | boolean; shift+tab stays thinking |
| KEY-015 | `app.mode.cycle` (alt+m) | oxa only: plan -> workspace -> ask |
| KEY-020 | `app.models.toggleProvider` (ctrl+p) | toggles all models of one engine endpoint |

## ENV: Environment variables

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| ENV-003 | Shell tool env `OXA_PROVIDER`, `OXA_MODEL`, `OXA_REASONING_LEVEL` | `OXA_MODEL`, `OXA_REASONING_LEVEL`; `OXA_PROVIDER` is always `oxe`. Set per run, never on the wire |
| ENV-007 | `OXA_OFFLINE` | accepted as `OXA_OFFLINE`; the same hard switch as `--offline` (truthy: 1, true, yes) |
| ENV-016 | `HTTP_PROXY`, `HTTPS_PROXY` | MCP HTTP only |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| ENV-008 | `OXA_SKIP_VERSION_CHECK`, `OXA_SELF_UPDATE_COMMAND`, `OXA_TELEMETRY` | no version check, self-update or telemetry |
| ENV-010 | `OXA_CACHE_RETENTION` | engine-side cache; no retention knob |
| ENV-011 | `OXA_SHARE_VIEWER_URL`, `OXA_RADIUS_GATEWAY` | no uploads |
| ENV-014 | `OXA_TUI_ESC_TIMEOUT` (default 100 ms) | pi-specific, not in oxa: one constant ESC timeout (10 ms), no variable |
| ENV-017 | Provider API key variables (`ANTHROPIC_API_KEY`, ...) | no cloud providers |

## TOOLS: Built-in tools

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| TOOLS-002 | `read`: output limited to 2000 lines or 50 KB | 2000 lines or the 16 KiB cap |
| TOOLS-003 | `read` on an image returns the image | returns the fixed placeholder line |
| TOOLS-004 | `read` on a binary file | refused with a fixed error (oxa only) |
| TOOLS-011 | `bash`: output truncated to the last 2000 lines or 50 KB, full output in a temp file | head + tail + one fixed line naming a spill file (sha256 name) |
| TOOLS-012 | `bash`: timeout kills the process tree; background-writer case | pi's text without the `after S seconds` clause (no timings in model-facing text) |
| TOOLS-013 | `bash` runs unsandboxed with the user's rights | bwrap sandbox; no bwrap = bash refuses; mode decides writable/net |
| TOOLS-014 | `grep`: schema `pattern`, `path`, `glob`, `ignoreCase`, `literal`, `context`, `limit` (100); .gitignore; 500-char lines; notices | in-process (`ignore` + `regex`), sorted, deterministic; no rg download |
| TOOLS-015 | `find`: `pattern`, `path`, `limit` (1000); glob relative to path; .gitignore | in-process, sorted; no fd download |
| TOOLS-022 | Same input + file state = same bytes; no ANSI, no timings (two runs identical) | (oxa only) |
| TOOLS-023 | MCP, extension and codemode results pass the same `shape` | (oxa only) |
| TOOLS-024 | (oxa only) `web_search`: a Brave Search tool, declared only with a key and never offline; pi has no built-in search | the tool list is frozen per prefix epoch; output is deterministic and capped, with source URLs |

## USE: Using the terminal UI

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| USE-010 | Drag-and-drop / paste of image and file paths | image: stored + shown, placeholder on the wire |
| USE-012 | Footer: context %, model, thinking, cwd | adds cache hit, prefill progress, tok/s, mode |

## SLASH: Slash commands

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| SLASH-002 | `/model [provider/model]` | engine models; new epoch |
| SLASH-003 | `/thinking [level]` | boolean |
| SLASH-004 | `/scoped-models` | engine models |
| SLASH-007 | `/llama` | `/engine`: status, load |
| SLASH-012 | `/tree` | a leaf move is a rebase; optional branch summary |
| SLASH-015 | `/compact [instructions]` | summary request |
| SLASH-020 | `/bug [description]` | writes a local redacted bundle; no upload |
| SLASH-023 | `/reload` | unchanged resources = no new epoch; changed = one new `Prefix` entry + cost notice |
| SLASH-027 | `/mode [plan\|workspace\|ask]` | oxa only |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| SLASH-005 | `/login [provider]` | `unsupported_in_oxa` |
| SLASH-006 | `/logout` | `unsupported_in_oxa` |
| SLASH-016 | `/import <path>` | omitted (removed with pi import) |
| SLASH-019 | `/share` | no third-party upload from a local-first tool |
| SLASH-025 | `/changelog` | pi-specific, not in oxa: `/changelog` removed |

## SES: Sessions

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| SES-012 | Automatic compaction keeps long sessions usable | automatic compaction at the context limit, in idle time when possible |
| SES-015 | `/bug` prepares a report | local redacted bundle |
| SES-016 | Torn last line of a session file is dropped on load | oxa only |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| SES-008 | `/import <path>` preserves id and cwd | omitted (removed with pi import) |
| SES-014 | `/share` uploads a session | no uploads |

## FMT: Session file format

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| FMT-003 | `SessionMessageEntry` (`type:"message"`) with `AgentMessage` (user, assistant, toolResult) | assistant entries also store the exact wire bytes (`args_raw`, untrimmed content, reasoning) for |
| FMT-004 | Leading system message (`role:"system"`, `sections`, tool declarations) as a `message` entry | the frozen prefix of an epoch; replayed byte for byte on resume |
| FMT-005 | Later system messages with section patches / `toolsRemoved` mid-session | no mid-session prompt change: a change is a new epoch entry |
| FMT-008 | `UsageEntry` (`kind`, usage) | kinds: `chat`, `compaction`, `branch_summary`, `prefill`; a subagent child runs without a session file, so its usage is not recorded in the parent (pi lists `subagent`) |
| FMT-009 | `CompactionEntry` (`summary`, `firstKeptEntryId`, `tokensBefore`, `details`, `fromHook`, `systemMessage`) | head after compaction is `[system, user:<summary>, tail]`; pi's `systemMessage` is not written, the epoch's prefix entry is the checkpoint |
| FMT-010 | `ContextEditEntry` | oxa reads the entry type and applies a stored one when the session is loaded (replacement and omission reach the wire: a rebase); oxa itself never writes one |
| FMT-018 | Context building: compaction replaces entries before `firstKeptEntryId` | rebase |
| FMT-020 | `ImageContent` blocks in user messages | `Part:Image` stored; never serialised to the wire |
| FMT-021 | oxa entry types `shown` and `mode_change` | oxa only: aborted/failed/cut replies and mode changes; ignored by pi tooling |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| FMT-019 | Session versions 1 and 2 are migrated on load | omitted (removed with pi import) |

## TREE: Tree navigation and branching

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| TREE-004 | Leaf move changes the context sent to the model | exactly one rebase; the engine restores from its nearest checkpoint and oxa prefills the rest in idle time |
| TREE-006 | Branch summary covers old leaf back to the common ancestor, newest-first within budget | request = the abandoned branch's wire + one instruction (pure append; foreground, the user waits on it) |
| TREE-012 | Moving back to an earlier leaf reuses the engine cache | oxa only: stub-contract test (tokens extend) |

## CMP: Compaction

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| CMP-001 | Auto compaction triggers when context exceeds `contextWindow - reserveTokens` | soft limit crossed at run end -> compact in idle time; mid-run only above the hard limit |
| CMP-002 | `keepRecentTokens` tail kept verbatim; cut-point rules (never between a tool call and its result) | default 4000; cut-point rules kept |
| CMP-003 | Split user-message spans: turn-prefix summary + history summary | replaced by the mid-run path; a prompt is never folded |
| CMP-004 | Summary generated by a separate request with a summarizer system prompt over serialized text | whole wire + one user instruction, same tools, thinking off (pure append); background only for the threshold case, foreground for `/compact`, overflow recovery and the compaction before a model switch |
| CMP-010 | Overflow recovery: compact once and retry the prompt | a failed reply is not on the wire; one recovery retry |
| CMP-015 | `compaction.modelOverrides` per model | engine model ids. an override naming another model than the session's is ignored with a warning; the summary runs on the session model (one slot: a swap unloads the model and cold-prefills the whole wire on the other one) |
| CMP-018 | After compaction the new head is prefilled in idle time (`max_tokens:0`, `X-Oxe-Snapshot: keep`) | oxa only; only when the engine advertises `/props` `oxe_hints.prefill_only`, else skipped |
| CMP-019 | The next prompt after a compaction is delivered live | oxa only (hardware gate 6.6) |

## JSON: JSON event stream

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| JSON-012 | Message `usage` object (`input`, `output`, `cacheRead`, `cacheWrite`, `totalTokens`, `cost`) | exact engine counts; `cacheWrite` and `cost` are 0 |
| JSON-026 | oxa events `prefill`, `prefix_epoch`, `cache_miss`, `mode_changed`, `notice` | oxa only; unknown to pi clients, which ignore unknown types |
| JSON-028 | JSON mode: a permission Ask is a Deny | oxa only (no dialog available) |

## RPC: RPC mode

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| RPC-012 | `get_state` | adds `mode` and cache fields |
| RPC-014 | `set_model` | engine models; new epoch |
| RPC-015 | `cycle_model` | engine models; new epoch |
| RPC-016 | `get_available_models` | engine models; Model object fields that do not apply are fixed values |
| RPC-017 | `set_thinking_level` | boolean |
| RPC-018 | `cycle_thinking_level` | boolean |
| RPC-019 | `get_available_thinking_levels` | clamped to the model |
| RPC-022 | `compact` |  |
| RPC-028 | `get_session_stats` | adds exact cache statistics; cost is 0 |
| RPC-039 | `set_mode` | oxa only: plan \| workspace \| ask |
| RPC-040 | Model object shape | one provider; costs 0; `contextWindow` = `n_ctx` |
| RPC-051 | A permission Ask is relayed as an extension-UI `confirm` | oxa only |
| RPC-052 | Rust RPC client drives the `examples/rpc-client` flow | no TS SDK; `oxa-rpc` ships a Rust client |

## TUI: Terminal UI

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| TUI-007 | Mermaid diagrams | shown as a highlighted code block |
| TUI-008 | LaTeX rendering | shown as source text |
| TUI-009 | Hyperlinks (OSC 8) with detection | detection only, no override |
| TUI-012 | Inline images: kitty and iTerm2 protocols with detection | display only; detection only, no override |
| TUI-020 | Keyboard: kitty protocol, legacy sequences, ESC timeout | kitty keyboard protocol flags 7 with the query; no modifyOtherKeys fallback; one constant ESC timeout of 10 ms counted from the ESC |
| TUI-021 | Terminal restored after a panic or signal | oxa only |
| TUI-023 | tmux setup (passthrough) | OSC 52 passthrough; extended keys through tmux are not in oxa |
| TUI-025 | Custom screens, overlays and custom editor from extensions | JS component `render(width)` + `handleInput`; pi-tui class shim |
| TUI-026 | Footer / status line shows mode, cache hit, prefill progress and tok/s | oxa only |
| TUI-027 | Permission prompt Once / Session / Deny; mode cycle; timed dialogs | oxa only |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| TUI-011 | Terminal progress (OSC 9;4) and program status (OSC 7501) | pi-specific, not in oxa: no terminal progress or program-status reports |
| TUI-024 | Windows / Termux / macOS terminal notes | Linux only |

## CFG: Configuration files

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| CFG-003 | Context files `AGENTS.md` / `CLAUDE.md`: agent dir, then parents of cwd down to cwd | part of the frozen prefix; below HOME the upward walk ends at HOME (pi walks to `/`) |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| CFG-007 | `auth.json` | no credentials store |
| CFG-008 | pi directories are not read implicitly | omitted (removed with pi import) |

## PKG: Packages

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| PKG-005 | Package manifest (`pi` key in package.json) and conventional directories | `oxa` key, `pi` key accepted |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| PKG-003 | `npm:` sources | pi-specific, not in oxa: no npm package sources; `npm:` is refused with `unsupported_in_oxa`, use a git URL or a local path |
| PKG-008 | Package dependencies on npm modules | no Node runtime: a package that needs Node modules is rejected with a clear error |
| PKG-009 | Package changelog display | pi-specific, not in oxa: no package changelog display |
| PKG-010 | Install telemetry | none |

## SEC: Security and project trust

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| SEC-001 | Trust prompt for project-local resources on first run in a directory | oxa is stricter: trust approves content (sha256 of the project files that run or load code); a changed file asks again (interactive) or is not loaded with a notice (headless) until approved or `--approve` |
| SEC-006 | pi has no sandbox and no permission prompts | built-in bwrap sandbox + modes plan / workspace / ask; trust gates what loads, modes gate what tools do; a deny-list of secrets (keys, tokens, the agent dir, browser profiles) is refused to the file tools and masked for bash in every mode; `.oxa/**` writes ask; `.git/hooks` and `.git/config` are never written by tools |
| SEC-007 | Containerization recipes (Docker, OpenShell, Gondolin) | replaced by the built-in sandbox |
| SEC-008 | Extension host functions and MCP stdio servers | pass the same `Policy` |

## CM: Codemode

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| CM-003 | `tools.<name>(args)` resolves to the tool's result | a script gets the same shaped result text the model would get; a failing tool rejects with an `Error` carrying it; no structured `bash` or `CallToolResult` objects |
| CM-010 | Output cap keeps start and end; full text in a temp file | `shape` |
| CM-016 | `codemode.mode` `on\|only` | the description is one fixed text, frozen per epoch; no inline tool listing, so no `codemode.inlineBudget` |
| CM-017 | `tool_search` declares matched tools for the next call | matches returned as text; called through one frozen `call_tool {name, args}`; prefix sha unchanged |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| CM-008 | `image(value)` | `unsupported_in_oxa` |
| CM-009 | `models.list/classify/generateImage` | `unsupported_in_oxa` |

## MCP: MCP servers

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| MCP-002 | stdio servers: `command`, `args`, `env`, `cwd` | spawned under the same `Policy` / sandbox rules |
| MCP-011 | Exposure `deferred` | via `tool_search` text + `call_tool` |
| MCP-012 | Exposure `direct` | frozen at epoch start; a server that connects later is offered at the next epoch |
| MCP-014 | `description` and the `mcp_servers` system prompt section (4096 / 250 char limits) | part of the frozen prefix; changes at the next epoch |
| MCP-015 | Resource tools `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` | text-only engine: image / audio / binary content blocks of any MCP result become the fixed placeholder line |
| MCP-022 | Permissions: MCP tool calls | workspace mode asks for MCP write-class tools |
| MCP-025 | MCP from the SDK | Rust API of `oxa-mcp` |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| MCP-023 | `pi.registerMcpServer` / `unregisterMcpServer` / `getMcpServers`; `mcp_servers_change` | pi-specific, not in oxa: MCP servers come from `mcp.json` / `oxa mcp add` only; the extension calls throw `unsupported_in_oxa` |
| MCP-024 | Replacing the built-in MCP support by an extension | built in (Rust); `-ne -e builtin:mcp` selection kept |
| MCP-026 | MCP `sampling/createMessage` (a server asks the client's model) | the engine has one inference slot and a server may not take it; the capability is not advertised and the request gets JSON-RPC -32601; the manager never calls the engine |

## EXT: Extensions

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| EXT-001 | Extensions are TypeScript modules loaded in process (jiti); `export default function (pi)` | JavaScript in embedded QuickJS, same API shape: `export default function (oxa)`; `.ts` type-stripped at load or rejected with a hint |
| EXT-010 | event `mcp_servers_change` | observer; server set changes apply at the next epoch |
| EXT-021 | event `before_agent_start` | may append a message; a system prompt replacement is refused |
| EXT-026 | event `ui_prompt_start` | fired around oxa's own permission prompt in every mode (pi has no permission prompt; its print mode has no UI) |
| EXT-027 | event `ui_prompt_end` | fired around oxa's own permission prompt in every mode (pi has no permission prompt; its print mode has no UI) |
| EXT-036 | event `context` | result must have the current wire as a byte prefix, else refused unless `rebase: true` (logged as a miss cause) |
| EXT-037 | event `context_with_system` | same rule; the system part is read-only |
| EXT-039 | event `before_provider_request` | same rule as `context` |
| EXT-040 | event `after_provider_response` | observer; `status` is 200 and `headers` is `{}` (the engine client does not hand them up) |
| EXT-041 | event `provider_stream_event` | observer; `data` is oxa's engine increment, not a raw provider chunk |
| EXT-042 | event `cache_warming_decision` | may cancel the idle prefill |
| EXT-051 | `ctx.ui.custom` components, overlays, custom editor | JS components via the pi-tui class shim |
| EXT-055 | `ctx.modelRegistry` / `ctx.model` / `ctx.thinkingLevel` / `ctx.scopedModels` | engine models only |
| EXT-059 | `ctx.compact` |  |
| EXT-060 | `ctx.getSystemPrompt` / `getSystemPromptOptions` | returns the frozen prefix; read-only |
| EXT-061 | `ctx.waitForIdle`, `ctx.newSession`, `ctx.fork`, `ctx.navigateTree`, `ctx.switchSession`, `ctx.reload` | session-shape actions requested mid-run apply when the run settles; `ctx.newSession` / `ctx.switchSession` answer `unsupported_in_oxa` (the user's `/new` and `/resume` do it) |
| EXT-063 | `pi.registerTool(definition)` with parameter schema validation | JSON Schema objects (no TypeBox import); tools join the frozen list at the next epoch |
| EXT-073 | `pi.exec(command, args, options)` | through `Policy` + sandbox |
| EXT-074 | `pi.getActiveTools` / `getAllTools` / `setActiveTools(names)` | a change is a new epoch, applied when the run settles |
| EXT-075 | `pi.setModel(model)` | engine models; new epoch |
| EXT-081 | Host `fs` and `fetch` functions | oxa only: no ambient authority; behind `Policy` |
| EXT-083 | Overriding built-in tools by name | allowed; frozen per epoch |
| EXT-084 | Tool output truncation helpers | `shape` applies to every extension tool result |
| EXT-085 | Dynamic tool loading mid-session | deferred to the next epoch; `tool_search` + `call_tool` for discovery |
| EXT-090 | Memory and time limits per extension context | oxa only |
| EXT-091 | Exec hooks: `settings.json` `hooks: {event: command}` | oxa only |
| EXT-092 | Subagent example extension (single / parallel / chain) | `builtin:subagent` (on by default on large non-Flash models; off on the Rust Flash engine and below 32K; `oxa.subagent.enabled` true/false wins; a member of the default tool selection): one child at a time, each `oxa --mode json` with its own session id, background; the parent is parked first; at most 8 children per parent session; see .. |
| EXT-093 | Example ports: plan-mode, permission-gate, custom-compaction, todo, status-line | each loads and passes a scripted session. pi's custom-compaction summarises with a second (cloud) model; the port sends no model request of its own (one model, one slot) and supplies custom instructions to the summary request |
| EXT-095 | TypeScript SDK (sdk.md) | the Rust API of `oxa-core` is the SDK; other languages use RPC |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| EXT-003 | Available imports (`@earendil-works/pi-*`, Node built-ins, npm dependencies) | no Node/npm imports; host functions only |
| EXT-038 | event `before_provider_headers` | one engine; headers are the fixed hints |
| EXT-056 | Streaming model calls from extensions | no side requests; `unsupported_in_oxa` |
| EXT-078 | `pi.registerProvider` / `unregisterProvider` | `unsupported_in_oxa` |
| EXT-079 | `pi.registerVirtualModel` / `unregisterVirtualModel` | `unsupported_in_oxa` |
| EXT-096 | Examples needing providers, Node modules or macOS (custom-provider-*, with-deps, mac-system-theme, ssh, gondolin) | out of scope |

## ENG: Engine support (oxa only)

### Adapted

| id | behaviour | how oxa differs |
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

## OUT: Out-of-scope pages

### Adapted

| id | behaviour | how oxa differs |
|---|---|---|
| OUT-011 | quickstart.md, index.md, how-pi-works.md | ported as oxa docs; every command line in them runs against the fake |

### Not in oxa

| id | behaviour | reason |
|---|---|---|
| OUT-001 | providers.md: cloud provider setup | one provider type: the oxe engine |
| OUT-002 | custom-provider.md |  |
| OUT-003 | virtual-models.md |  |
| OUT-004 | models.md / models-configuration.md beyond the rows in MOD | catalogs, costs and provider options do not apply |
| OUT-005 | llama-cpp.md, nvidia-rtx-llama-cpp.md | replaced by the engine's own docs and `/engine` |
| OUT-006 | windows.md | Linux aarch64/x86_64 only |
| OUT-007 | termux.md | Linux aarch64/x86_64 only |
| OUT-008 | containerization.md | replaced by the built-in sandbox (see SEC) |
| OUT-009 | shell-aliases.md | `!cmd` runs `bash -c` with the user's environment; no alias loading |
| OUT-010 | cli-integration.md (wrapping other CLIs with provider credentials) | no provider credentials |
