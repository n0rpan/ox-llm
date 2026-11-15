# Command line

```text
oxa [options] [--] [@files...] [messages...]
```

With no message, `oxa` opens the interface ([usage.md](usage.md)). With a message and `-p`, it runs the prompts and exits. Each `message` after the first is its own prompt. `@file` arguments are attached to the first prompt, and piped standard input is added to it.

In `-p` mode, standard output holds only the final reply and a newline. Progress goes to standard error as the run goes, flushed per event: the text of each message that is followed by tool calls, and one line per tool call (`[tool] bash: <first 100 characters of the command>`, then `[tool] bash ok 1.2s` or `failed`). A reply that is followed by another turn (a failed `done` check, a steering message) is progress too. The final reply is written once, when the run ends, and not at all when the run failed or was aborted. `--mode json` prints no progress lines; it writes one flushed record per event.

Print mode saves a session by default and announces its id on stderr before starting the agent. Continue that exact session with a new instruction:

```console
$ oxa -p "start the task"
$ oxa -p --continue SESSION_ID "continue with this instruction"
```

Replace `SESSION_ID` with the printed id. `--continue=<id>` also works. The id is consumed as an option value and never sent as a prompt. `--continue` without a value (or `-c`) selects the most recent session; put a following prompt after `-p` to leave the id omitted, for example `oxa --continue -p "next instruction"`. `--no-session` keeps the run ephemeral and prints no session id. An explicit continuation id cannot be combined with `--no-session` or `--session`.

Print, JSON and RPC runs write exactly one terminal JSON result on standard error, after diagnostics. For example:

```json
{"type":"oxa_result","outcome":"blocked","error_class":"budget_exhausted","exit_code":3}
```

The `outcome` is `done`, `nothing_to_do`, `blocked`, `failed` or `interrupted`. `error_class` is a stable error code (for example `engine_http`, `stream_broken`, `done_checks_failed` or `output_limit`), or `null` for successful and no-op runs. Diagnostics remain human-readable; callers should use the result and exit code instead of matching their wording.

| Exit code | Outcome | Meaning |
|---|---|---|
| 0 | `done` | The requested run completed |
| 1 | `failed` | Runtime failure, failed done checks or truncated output |
| 2 | `failed` | Usage error, unsupported option or engine unavailable |
| 3 | `blocked` | Daily cloud budget exhausted (`budget_exhausted`) |
| 4 | `nothing_to_do` | No agent run occurred, for example input handled by a hook, or RPC EOF without work |
| 5 | `blocked` | Cloud credentials unavailable (`cloud_credentials_unavailable`) |
| 6 | `blocked` | Cloud ledger unavailable (`cloud_ledger_unavailable`) |
| 7 | `blocked` | Offline policy refused the request (`offline`) |
| 8 | `blocked` | Other setup refusal, such as `cloud_sdk_unavailable` |
| 130 | `interrupted` | The run was cooperatively cancelled, for example by RPC `abort` |

For multiple prompts, the first unsuccessful outcome is retained even if a later prompt succeeds. RPC continues accepting commands after a run fails; the process reports its aggregate outcome when stdin closes. Reader-side rejections and malformed commands count as failures, and a later valid command does not clear them. Each RPC `agent_settled` record also gains an additive `result` field with that prompt's result. A handled RPC command that starts no agent turn counts as completed work; a handled prompt that starts no turn counts as `nothing_to_do`.

These results cover orderly returns, including startup refusals. A process killed before it can return cannot emit a terminal result. Help, version, export, model listing and standalone subcommands keep their existing output and status conventions (0 success, 1 runtime failure, 2 usage error).

Retries use the `retry` settings: transient gateway credentials/ledger refusals, upstream 429/5xx and connection failures retry with capped exponential backoff before output starts. Budget exhaustion, missing SDKs and other 4xx refusals are final. Once output has streamed, a failure is returned without replaying the request. Print mode immediately flushes a one-line `retrying request in ... ms` notice to stderr; JSON and RPC modes emit it as a flushed `notice` record. Provider-level retries (`retry.provider.maxRetries`) are visible too. All attempts for a logical request reuse its idempotency key. `retry.enabled`, `retry.maxRetries`, `retry.baseDelayMs` and `retry.maxAgentDelayMs` control the agent-level policy; provider retries have their own configured limit.

`--max-time <seconds>` sets a soft runtime limit for print, JSON and RPC modes, measured after startup/session loading. Fractional seconds are accepted. At the limit, or on SIGTERM, oxa lets the active tool batch finish, then gives the agent one final turn (up to 1,024 output tokens, thinking off) to commit changes from this run or summarize its state. That final request is not retried by the agent. The session and completed tool results are retained; the outcome is `interrupted` (130), and later prompts are left unstarted. Resume with the session id and a new instruction. RPC also exits on a limit/SIGTERM while stdin is still open. `--no-session` continues to disable session storage.

This is a graceful limit: an active tool or model request finishes before the final turn, so elapsed time can exceed the requested duration. SIGTERM does not cancel that tool. SIGKILL and process/system crashes cannot run a wrap-up turn.

## Commands

Commands take the place of the message. They do not start a session.

| Command | What it does |
|---|---|
| `oxa install <source> [-l]` | add a package (a git URL or a local path) to the settings and install it |
| `oxa remove <source> [-l]` | remove a package from the settings (`uninstall` is an alias) |
| `oxa update [source] [--extensions]` | update installed packages, all of them or the one named |
| `oxa list` | list the installed packages |
| `oxa config [-l]` | print every setting with its value and where it comes from |
| `oxa mcp <command>` | manage MCP servers, see below |
| `oxa identity [--refresh] [--offline]` | show the identity text of the system prompt; `--refresh` rewrites it from this machine and the engine, and `--offline` skips the engine |
| `oxa report-issue <draft\|submit>` | draft a scrubbed issue for the ox-llm repository, or post a draft after confirmation |
| `oxa wipe --session <id> \| --all [--yes] [--engine]` | remove the stored session files and spill files of one session or of all sessions. See [sessions.md](sessions.md#what-oxa-stores-and-how-to-remove-it) |

`-l` (`--local`) writes to the project's `.oxa/settings.json` instead of the user settings. Package sources must be git URLs or local paths. npm sources are refused.

`oxa update --self`, `--all` and `--models` report that they are not supported: oxa does not update itself and does not refresh model catalogues. Update oxa by installing a new build.

`oxa report-issue draft` takes `--title <text>` (required), and the notes either inline with `--notes <text>` or from a file with `--notes-file <path>` (relative paths are taken from the current directory). It writes the draft under `issue-drafts/` in the agent directory and prints its path; it posts nothing. `oxa report-issue submit <draft>` posts it after you type `yes`.

`oxa config` lists the settings as oxa resolves them.

### MCP commands

```console
$ oxa mcp add <name> [-l] [options] -- <command> [args...]
$ oxa mcp add <name> [-l] --url <url> [options]
$ oxa mcp remove <name> [-l]
$ oxa mcp list [--json]
$ oxa mcp login <name> [--timeout <seconds>]
$ oxa mcp logout <name>
```

- `add` options for a stdio server: `--env KEY=VALUE` (repeatable), `--cwd <dir>`, `--description <text>`, `--exposure <mode>`.
- `add` options for an HTTP server: `--url <url>`, `--header KEY=VALUE` (repeatable), `--bearer-token-env-var <VAR>`, `--oauth-client-id <id>`, `--oauth-client-secret <secret>`, `--oauth-callback-port <port>`, `--oauth-client-name <name>`, `--oauth-client-registration auto|cimd|dcr`.
- `--exposure` takes `codemode` (default), `deferred`, `direct` or `hidden`. See [mcp.md](mcp.md).
- `list` connects to every enabled server and reports its state, transport and tools. `--json` prints the same as JSON.
- `login` connects to the server and runs the OAuth sign-in. The default timeout is 300 seconds. `logout` deletes the stored credentials.

## Options

### Model and output

| Option | Meaning |
|---|---|
| `--provider <name>` | provider to search for `--model`; requires `--model` |
| `--model <pattern>` | model pattern or id; accepts `provider/id` and a `:<thinking>` suffix |
| `--models <patterns>` | comma-separated model patterns for the Ctrl+P cycle |
| `--api-key <key>` | bearer key for the engine endpoint of this run; not stored |
| `--list-models [search]` | list the models the engine serves, optionally filtered by a fuzzy search, and exit |
| `--thinking <level>` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max` |
| `--mode <mode>` | output mode: `text` (default), `json` or `rpc` ([rpc.md](rpc.md)) |
| `--print`, `-p` | non-interactive: process the prompts and exit |
| `--max-time <seconds>` | softly limit a headless run, then allow one final wrap-up turn |
| `--export <input> [output]` | export a session file to HTML and exit |
| `--tui-mode <mode>` | `fullscreen` (default) or `regular` |
| `--verbose` | show more start-up information |

### Sessions

| Option | Meaning |
|---|---|
| `--continue [id]`, `-c` | continue the named session; without an id, continue the most recent session of the current directory |
| `--resume`, `-r` | pick a session to resume |
| `--session <path\|id>` | open a session by file path, or by full or partial id |
| `--session-id <id>` | use the current directory's session with this id, creating it if needed |
| `--fork <path\|id>` | fork a session into a new session in the current directory |
| `--session-dir <dir>` | directory for session storage and lookup |
| `--no-session` | do not save the session (ephemeral) |
| `--name`, `-n <name>` | set the session's display name |

See [sessions.md](sessions.md).

### Tools

| Option | Meaning |
|---|---|
| `--tools`, `-t <list>` | allowlist of tool names or patterns (`*` matches any characters) |
| `--exclude-tools`, `-xt <list>` | denylist of tool names or patterns |
| `--no-builtin-tools`, `-nbt` | disable the built-in tools by default; extension tools stay |
| `--no-tools`, `-nt` | disable all tools by default |
| `--no-mcp` | no MCP servers connect and no MCP tools are offered |
| `--no-web-search` | disable `web_search` (same as `OXA_WEB_SEARCH=0`) |

See [tools.md](tools.md) for the tool names.

### Permissions and trust

| Option | Meaning |
|---|---|
| `--permission-mode <mode>` | `plan`, `workspace` (default) or `ask` |
| `--yolo` | no permission prompts: every question is auto-approved; the sandbox and the deny-list stay on ([security.md](security.md#yolo-no-prompts)) |
| `--approve`, `-a` | trust the project's configuration and resources for this run |
| `--no-approve`, `-na` | ignore the project's configuration and resources for this run |
| `--offline` | offline mode: oxa's own non-local connections are refused, bash has no network, `web_search` is off, and `!cmd` and stdio MCP servers run without network. What it does not cover is in [security.md](security.md#offline-mode) |

See [security.md](security.md).

### Extensions, skills, prompts, themes

| Option | Meaning |
|---|---|
| `--extension`, `-e <path>` | load an extension file, or `builtin:<name>` (repeatable) |
| `--no-extensions`, `-ne` | disable extension discovery and built-in extensions |
| `--skill <path>` | load a skill file or directory (repeatable) |
| `--no-skills`, `-ns` | disable skill discovery and loading |
| `--prompt-template <path>` | load a prompt template file or directory (repeatable) |
| `--no-prompt-templates`, `-np` | disable prompt template discovery and loading |
| `--theme <path>` | load a theme file or directory (repeatable) |
| `--use-theme <name[/name]>` | set the interactive theme for this run |
| `--no-themes` | disable theme discovery and loading |
| `--no-context-files`, `-nc` | disable `AGENTS.md` and `CLAUDE.md` discovery |
| `--system-prompt <text\|path>` | replace the system prompt with text or with a file's contents |
| `--append-system-prompt <text\|path>` | append text or a file to the system prompt (repeatable) |

Options added by extensions are listed after the options when you run `oxa --help`, under "Extension CLI Flags".

### Information

| Option | Meaning |
|---|---|
| `--help`, `-h` | show the help |
| `--version`, `-v` | show the version |
| `--` | end option parsing; the rest are messages and files |

## Environment variables

Variables that oxa reads. Variables marked *set* are set by oxa for the bash tool and its children, and you do not need to set them.

| Variable | Meaning |
|---|---|
| `OXA_ENGINE_URL` | engine endpoint for the default model entry. Default `http://127.0.0.1:8092` |
| `OXA_AGENT_DIR` | agent directory. Default `~/.oxa/agent`; `~` is expanded |
| `OXA_SESSION_DIR` | session storage; `--session-dir` wins over it |
| `OXA_PACKAGE_DIR` | package directory, for Nix or Guix store paths |
| `OXA_SYSTEM_SKILLS_DIR` | system skills directory, read last. Default `/usr/share/omarchy/default/agents/skills`; empty disables it |
| `OXA_OFFLINE` | `1`, `true` or `yes` turns offline mode on, as `--offline` does |
| `OXA_ALLOW_HOME_CWD` | `1`, `true` or `yes` allows a start in the home directory, `/` or a parent of home, as the user setting `oxa.allowHomeCwd` does. See [security.md](security.md#starting-in-your-home-directory) |
| `OXA_WEB_SEARCH` | `0`, `false`, `no` or `off` disables `web_search` |
| `BRAVE_SEARCH_API_KEY` | Brave Search key for `web_search`; wins over `brave-search.key` ([tools.md](tools.md#web_search)) |
| `OXA_CODEMODE_MAX_WORKERS` | script workers alive at once in the process. Default 2; overrides `codemode.maxWorkers` |
| `OXA_CLEAR_ON_SHRINK` | `1` clears empty rows when the rendered output shrinks |
| `OXA_SKIN` | `pi`, `oxa` or `ascii` (ASCII glyphs only): the look of the interactive UI for this run, over the `ui.skin` setting |
| `OXA_REDUCED_MOTION` | `1` switches the animations of the `oxa` skin off, as the `ui.reducedMotion` setting does |
| `NO_COLOR` | with the `oxa` skin: no colour and no animation |
| `OXA_CLIPBOARD_OSC52` | `1` also copies through OSC 52, the escape sequence that terminals relay to the clipboard |
| `VISUAL`, `EDITOR` | external editor for Ctrl+G, when `externalEditor` is not set; `VISUAL` first |
| `HTTP_PROXY`, `HTTPS_PROXY` | proxy for MCP HTTP requests only |
| `OXA_SESSION_ID`, `OXA_SESSION_FILE` | *set*: the current session id, and the session file (unset when the session is ephemeral) |
| `OXA_PROVIDER`, `OXA_MODEL`, `OXA_REASONING_LEVEL` | *set*: the provider (always `oxe`), the model id and the effective thinking level |
| `OXA_CODING_AGENT`, `AI_AGENT` | *set*: process markers; `OXA_CODING_AGENT` is `true` and `AI_AGENT` is `oxa` in child processes |

Some settings name the same things (`httpProxy`, `externalEditor`, `sessionDir`). `OXA_SESSION_DIR` wins over `sessionDir`, and `BRAVE_SEARCH_API_KEY` wins over the key file. The rules for each setting are in [settings.md](settings.md).

## Examples

These run without a key and without network access against a local engine:

```sh
oxa --help
oxa --version
oxa -p "Say hello"
oxa --mode json -p "hi"
oxa -p --tools read,ls "List the files"
oxa -p --offline "hi"
```

Interactive use, and commands that need a key, an MCP server or an existing session, are shown in the other pages:

```console
$ oxa --continue
$ oxa --session-id 2f9c1a7e --name "parser work"
$ oxa --export ~/.oxa/agent/sessions/last.jsonl ~/last.html
$ oxa --permission-mode ask --tools read,grep,find,ls
$ oxa mcp add docs --url https://mcp.example.com/mcp --oauth-client-registration auto
$ oxa mcp login docs
```

`oxa --export` takes a session file, not an id. Find a session file under the session directory described in [sessions.md](sessions.md).
