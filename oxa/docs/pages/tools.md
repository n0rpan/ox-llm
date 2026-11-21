# Tools

The model acts through tools. oxa has seven file/command built-in tools and the lead-session `ask` tool, all on by default, and a few optional ones: `subagent`, `web_search`, `codemode`, `tool_search` and `call_tool`. Extensions and MCP servers add more. Every tool result is capped before it reaches the model.

See [tools and context handling](tools-context.md) for the defaults and formats.

## The built-in tools

| Tool | Access | What it does |
|---|---|---|
| `read` | read | reads a file. Parameters `path`, `offset` (first line, 1-based), `limit` (lines). A call returns at most 2000 lines or 16 KB; continue with `offset` for the rest. A call with neither `offset` nor `limit` returns a shorter first page; see [tools-context.md](tools-context.md) |
| `bash` | exec | runs a shell command in the project in the sandbox. Parameters `command`, `timeout` (seconds). With no `timeout` it waits 120 seconds, then moves to a background job instead of being killed; `oxa.bash.defaultTimeout` sets it, `oxa.bash.waitCap` (default 300 s) caps a job read's `wait_s`. Output over 16 KB is cut to its first and last parts, and the full output is saved to a file the model can read |
| `edit` | write | replaces exact text in one file. Takes one `oldText` / `newText` pair per call; the array form is still accepted. See [tools-context.md](tools-context.md) |
| `write` | write | writes a file, creating it and its parent directories, and overwrites an existing file |
| `grep` | read | searches file contents for `pattern`. Optional `path`, `glob`, `ignoreCase`, `literal`, `context` and `limit` (default 100 matches). Respects `.gitignore`; lines are cut at 500 characters. When matches exceed `limit`, the answer is the total and one count per file instead of the lines; see [tools-context.md](tools-context.md) |
| `find` | read | finds files by glob `pattern` under `path`, up to `limit` results (default 1000). Respects `.gitignore` |
| `ls` | read | lists a directory, sorted, directories with a trailing `/`, dotfiles included. Up to `limit` entries (default 500) |

The access class decides what permission mode asks for; see [security.md](security.md#permission-modes). Paths are resolved against the project directory, and `..` and symbolic links are followed before the check.

Tool names, descriptions and schemas are part of the system prompt. They stay the same during a session, and change only after `/reload`, a model switch or a new session.

### Output limits

- Any tool output sent to the model is capped at `oxa.toolOutputCap` bytes (16384 by default; on a cloud model 3/16 of its window in bytes unless you set it). Output above the cap is cut and says so. When `oxa.compaction.softTokens` is set or the engine publishes a compaction point, one tool result is at most a quarter of it (three bytes per token, never below 2048 bytes), unless `oxa.toolOutputCap` is set. Only the first page of a `read` (no `offset` or `limit`) is at most 8 KiB; `grep`, `find`, `ls` and a `read` with `offset` or `limit` are cut at the cap itself, with a note inside the cap that says how to go on. See [tools-context.md](tools-context.md).
- `read`, `grep`, `find` and `ls` apply their own line or match limits first.
- For bash, the full output is kept in a file, and the model is told where it is.

## Choosing tools

In a headless run (`-p`, `--mode json`) with the output diet on, the prompt adds one integrity rule: solve the task yourself, do not look up its ready-made solution online and do not read verifier, grader or hidden-test files. The `oxa_result` record reports `web_lookups`, the number of `web_search` calls. It is a prompt rule and a count, not a block: a tool call is never refused because of it.

| Option or setting | Effect |
|---|---|
| `--tools a,b` (`-t`) | allowlist of names or patterns; `*` matches any characters. Only these tools are on |
| `--exclude-tools a,b` (`-xt`) | denylist, applied after the allowlist. Repeated lists and `--no-<tool>` add to it |
| `--no-<tool>` | one exclusion by name: `--no-read`, `--no-bash`, `--no-edit`, `--no-write`, `--no-grep`, `--no-find`, `--no-ls`, `--no-subagent`; the same as `--exclude-tools <tool>` |
| `--no-builtin-tools` (`-nbt`) | disables the built-in tools; extension and MCP tools stay |
| `--no-tools` (`-nt`) | disables all tools |
| `--no-mcp` | no MCP servers connect and no MCP tools are offered |
| `defaultTools` setting | the list of tools on by default. Names may include `subagent`, `web_search`, `codemode` and `tool_search`, and a list can be patched with `+name` and `-name` |

With the output diet on (the default) the rules also tell the model to run the project's own checks before it calls a task done (when `bash` is on), and to use `wait` instead of polling (whenever `wait` is declared, which is every session unless `oxa.tools.wait` is `false`). `wait` takes seconds, and optionally `until` `ci`, `pr_merged`, `job_done` (a background bash job id) or a GitHub read probe; `ci`, `pr_merged` and the read probes take an optional `"repo":"OWNER/NAME"` (otherwise the origin of the working directory; github.com only); the GitHub kinds run `gh`, which is network access: plan mode denies them, ask mode asks once per session for `github.com`, workspace and yolo do not ask. A message the owner types ends it early.
In an interactive session with both tools enabled, wait for a native background worker started by that session with `{"seconds":60,"reason":"Worker review","until":{"kind":"worker_done","name":"review"}}`. The wait returns the worker’s outcome (not its log), or reports that it stopped or exited without an outcome. This does not queue a follow-up or interrupt a plain wait.
`wait` with `until` `pr_activity` waits for a review or conversation comment on a pull request you pinned at a head: `{"seconds":1800,"reason":"Review of #7","until":{"kind":"pr_activity","pr":7,"head":"<sha>"}}`. It wakes with the new reviews and comments (author, state, link, body cut to 1500 characters, at most 10 listed), and also when the head is no longer the pinned one or the PR is merged or closed. The first poll for a PR and head is the baseline: what is already there does not wake it, unless you pass `"since":"<UTC time like 2026-11-20T10:00:00Z>"`, which counts anything newer on that first poll (use it after a restart). The baseline lasts for the session, and evidence that was returned is not returned again.
A resumed session (`--continue`, `--resume`, `/resume`) keeps `wait` and, for a goal that is pursuing, `goal_done`, without opening a new prefix epoch. If a goal is pursuing but the tool selection (`--tools`, `defaultTools`) removed `goal_done`, the goal pauses with that reason instead of repeating a continue instruction the model cannot follow; add `goal_done` to the selection, then `/goal resume`.

Examples:

```sh
oxa -p --tools read,ls "List the files"
```

```console
$ oxa --tools 'read,grep,find,ls' --permission-mode ask
$ oxa --exclude-tools bash
```

A read-only session: `oxa --tools read,grep,find,ls`. In plan mode the write tools are refused, and bash only runs read-only commands with no network.

## Background parts and jobs

A command with a part that runs detached (an `&` outside quotes, anywhere in the command, or `nohup`, `setsid`, `disown`) gets a note in its result: that part's output and exit status are not in the result, a `cd` before the `&` applies only to the backgrounded part, and what happens to the process. In the OS sandbox every process of the call ends with the call; without it (yolo, or no sandbox) it keeps running and oxa does not track it. The call is marked `detached` and never counts as a passing check. To run something you can read, wait for or stop, use `bash` with `background: true` and then `bash` with `job` (and `wait_s`, or `kill`), or `wait` with `until` `job_done`.

## Before a run ends

One decision runs before a run or goal ends. Each open fact gets at most one extra turn per run, then the run ends with a warning and a record; nothing blocks.

- A run that changed files (a file tool, or a `bash` call that changed the work folder) and ran no passing check afterwards is asked to run the project's check, or to say in one line why none exists (`oxa.done.proveIt`, off by default). A read-only run is never asked. The `oxa_result` record carries `proved` (`true`, `false`, or `null` for a read-only run). The work-folder fingerprint that sees `bash` edits is now also taken when only this setting is on.
- A goal that completes, or a headless run that ends, while background jobs still run is asked to wait for them or stop them (`oxa.done.waitForJobs`); a headless run's jobs stop when it ends. An interactive chat is never asked, so a dev server costs no turn. The record carries `jobs_running`.
- When the task names the tool its own acceptance runs through (a line that calls it the grader, the
  harness, the verifier or the acceptance, or puts a version beside the tool word, as in `Python 3.12`
  or `PHP 8.2`), and no `bash` call of the run ran it, the run is asked once to run that tool or say
  which check did not run and what stays unverified. The question shows the first commands that named
  the tool, so a fallback is visible. With `oxa.done.proveIt` on, this question stands in for the edit
  proof. With `bash` not declared, or while a goal is pursued, nothing is read and nothing is asked. It
  never refuses, never installs anything and makes no network call, and the run still ends after its one
  turn. Nothing is said about a tool that merely is missing from `PATH`: only a task that names its own
  acceptance tool is checked. The record carries `verifier_unused`, the tool name when it never ran
  (`null` when it ran, or when the task names none).

## Repair turn

Some models write a tool call as text instead of a structured call, or write a malformed one. When a reply has no tool call but its text contains a tool-call opener (`<tool_call>` or `<function=`) outside a code block, oxa:

1. classifies the first such block: an unknown function (with the list of available tools), an unknown parameter (with the valid names), a missing required parameter, an unclosed block, invalid JSON, a wrong type, or a bad format;
2. appends one fixed user message that names the problem and asks for a corrected call. The message is stored as a hidden `custom_message` entry;
3. runs the next turn.

At most two repairs can follow each other. A valid tool call resets the count. The third malformed reply ends the run; the raw reply stays visible, with a stop message. Each repair shows a warning notice and adds to the footer count `repairs N`.

`oxa.toolRepair: false` turns repairs off, and a malformed reply then ends the run at once.

## Repeated calls

A model can get stuck making the same tool call again and again. When a call (any tool: `read`, `bash`, `grep`, `find`, `ls`, `web_search`, MCP and extension tools) has returned the same result three times in a row, with the same arguments and nothing else in between, oxa still runs it but sends a short note instead of the result: the result is unchanged and already in the conversation, change approach. Failing results count too. If the repeats go on, the second note is the last before a warning: the next identical call adds one warning to the transcript, shown and kept in the session, with the remedy (send a more specific instruction, or switch model with `/model`) — and the run continues, so a later reply can still finish the task. An `edit` or `write` that failed changed nothing, so it does not end the run: `read`, failing `edit`, `read`, failing `edit` is caught like the plain repeat. A re-run after a successful edit or any other call, a command whose output changed (a test after a fix, a polling command), or the same call after a new prompt is never affected.

`oxa.toolRepeatGuard: false` turns this off.

Loops that vary too much for this guard (calls that alternate, output with a timestamp), or one that keeps repeating after the warning, are not ended by it. To bound any of them, set `oxa.maxTurns` to a number of model replies: a run that reaches it ends with a `turn_limit` error. It is off by default.

## CI polling

A `bash` command that waits for CI by polling is not blocked. It runs and reports its own exit status (also a non-zero one: `gh pr checks` exits 8 or 1 while checks fail or are pending), and its result carries a note with the native call to use instead: `wait {"seconds":3600,"reason":"Wait for CI","until":{"kind":"ci","sha":"<head>","workflow":"<workflow>"}}`. The note fills in the 40-hex head commit and the workflow name when the command or its output names them, and leaves the placeholders otherwise. When the `wait` tool is not declared (`oxa.tools.wait` is `false`) the note says to run one blocking check instead of a loop.

A polling command is a `for` / `while` / `until` loop or `watch` around `gh run`, `gh workflow` or `gh pr checks`, `gh run watch` itself, `gh pr checks --watch`, or an earlier `sleep` before the check. Quoted text is ignored, and a single `gh run view`, `gh run list`, `gh pr view` or `gh pr checks` is not polling. `oxa.bash.ciPollHint: false` turns the note off. The call is marked `ciPoll`, and the `oxa_result` record carries `ci_polls`, the number of such calls, so a session that polls from bash can be seen. The `wait` tool's description names `ci` and `pr_merged` first with one example call; the tool list changes once with it.

## Thinking on tool follow-ups

Under `oxa.thinking.policy: auto` the first reply after your prompt thinks. The replies after tool results follow `oxa.thinking.toolFollowups`:

- `off`: they never think. Fast, but a model that does not think between steps can loop.
- `on`: they always think. Steady, but slow on long runs of routine steps such as paging through a large file.
- `auto` (the default): they think only while the run needs it. Thinking turns on for the next reply after:
  - a failed call: a command that exits non-zero, an edit that did not apply, any tool error, or (when your prompt asks to fix, debug or implement something) a test or build command whose output reports a failure;
  - a repeat note ([Repeated calls](#repeated-calls));
  - `oxa.thinking.escalation.fruitlessCalls` calls (default 6) without an edit or write since the prompt. Reading a new file or range counts as progress; re-reads, repeated calls and commands count.

  When your prompt asks to fix, debug or implement something, thinking then stays on until an edit or write succeeds or a test or build command passes. Only a test runner or build that the `bash` call actually runs counts (`cargo test`, `pytest`, `go test`, `npm test`, `make`, `tsc`, ...), and a test run passes only when it exits 0 and prints its pass summary; a search or read that merely mentions a test command (`grep -rn "cargo test"`, `ls tests/`) never does. For other prompts (questions, reviews, reading) it goes back off after the next step whose calls all succeed and are new, so a read-heavy task stays fast. A new prompt starts from off.

  The footer shows `thinking: auto→on` while it is on, and the transcript shows a note with the reason each time it turns on.

The thinking level `off` (Shift+Tab, `/thinking`) always wins: nothing turns thinking on. The policy `fixed` sends the level on every request. Switching thinking between requests changes no byte already sent, so the prompt cache is kept.

## ask

`ask` is for decisions that need the owner's input, not routine work. Its arguments are
`{question, options: [{label, detail}, ...], recommended}`: provide 2–4 distinct, non-empty
option labels with details, and use an exact option label for `recommended`.

- Interactive sessions show the existing selection dialog, with details and the recommended
  option marked. Escape dismisses it without approving or selecting the recommendation.
- RPC hosts with an owner present opt in with `OXA_RPC_OWNER_PRESENT=1` when launching oxa.
  Then `ask` emits the existing `extension_ui_request` with `method: "select"`; respond with
  the chosen option's displayed string using `extension_ui_response`. This capability affects
  only `ask`, not permission dialogs or extension UI. RPC without this opt-in is unattended.
- Print/JSON and unattended RPC sessions return immediately with `no owner; proceed on the recommended option`
  and append the question, options, recommendation, working directory and session ID to
  `~/.oxa/agent/asks.jsonl` (under `OXA_AGENT_DIR` if overridden). This private, append-only
  JSON Lines file lets an unattended runner collect questions for later review. A journal
  write failure is reported in the tool result but does not stall work.

Only lead sessions declare `ask`; subagent workers keep their existing tool prefixes.
`--exclude-tools ask`, an explicit tool allowlist, and `--no-tools` can disable it.
An answer or automatic recommendation never grants file, shell or network permissions:
normal permission checks remain in force.

## subagent

`subagent` lets the model hand a task to a child oxa that works in its own context and returns only its final answer. It keeps long searches out of the main conversation.

- Shapes: `{agent, task}` for one child; `{tasks: [...]}` for several, run one after another in order; `{chain: [...]}` for steps where each step can use `{previous}`, the output of the step before.
- Agents are Markdown files in `~/.oxa/agent/agents/` and, in a trusted project, `.oxa/agents/`. The file has a `name` and a `description` in its front matter, and its body is the child's system prompt. A project agent with the same name replaces the user agent. A `model:` in an agent file is used when that model or the session's model runs in the cloud; with a local session and a local `model:` it is ignored with a warning (one local model is loaded) and the child uses the session's model.
- The child is a new oxa process in JSON print mode, with the same model and permission mode, and no session file.
- **On a local model the child forks your session.** Its first request carries your frozen prefix (the system text and the tool list) and your whole conversation, with the task appended, so the engine continues the session's state instead of evicting it: the child prefills nothing of what you already have, and your next turn reuses its context (a local re-prefill costs 45-100 s at 12K tokens). Two things follow from sharing the prefix: your tool list is what the child is offered, so a tool its agent file does not list comes back as an unknown tool, and the agent's system prompt is appended to the task instead of the system prompt. A fork child's compaction point is the session's own ([compaction.md](compaction.md)). A cloud child, a cloud session and `oxa.subagent.isolated: true` keep the isolated child: its own session and prefix, the parent parked, and its own compaction limit, at the cost of that prefill. The call names that cost once when the setting is why.
- The result is the child's final text, capped at `oxa.subagent.resultCap` (6144 bytes; scaled with the window of a cloud model unless you set it). A longer result is cut and the full text is saved to a file under the agent directory's `tmp/`.
- Local children run one at a time. When children of a `tasks` call run on a cloud model, up to `oxa.subagent.remoteConcurrency` (8) run at once; results still come back in call order. A call with more tasks than `oxa.subagent.maxChildren` (8) still runs, queued, and its result ends with a warning.
- Interrupting the parent stops the child: SIGTERM to its process group, then SIGKILL after 2 seconds.
- Plan mode refuses the tool, because it runs a process.

The tool is on by default when the session's model is not Flash and its context window is at least 32K. `oxa.subagent.enabled` sets it explicitly, and `oxa.subagent.smallContext: true` allows it on a smaller window. A child cannot run `subagent` itself (depth 1); a fork child's shared prefix still lists it, and calling it there comes back as an unknown tool.

## web_search

`web_search` searches the web through Brave Search and returns numbered results with their URLs, dates and snippets. A query leaves your machine when the model calls the tool, and the query text is sent to Brave. See [security.md](security.md#web_search) for what that means.

### Setup

1. Create a Brave Search API key at [brave.com/search/api](https://brave.com/search/api).
2. Give oxa the key, in one of two ways:
   - the environment variable `BRAVE_SEARCH_API_KEY`, which wins over the file; or
   - a file `~/.oxa/agent/brave-search.key` containing the key. The file must be readable by you only:

```console
$ chmod 600 ~/.oxa/agent/brave-search.key
```

oxa refuses a key file that is readable by others, a symbolic link, a non-regular file or an empty file, and reports the reason at start-up. It never prints the key.

Brave's current terms, free allowances and prices are on [brave.com/search/api](https://brave.com/search/api). Check them there before you rely on a free tier.

Start oxa and run `/web-search` to see the provider, where the key came from, and the endpoint in use. The tool is offered to the model only when a key is configured.

### Turning it off

- No key: the tool is not declared at all.
- `--no-web-search`, or `OXA_WEB_SEARCH=0` (also `false`, `no` or `off`); `1`, `true`, `yes` and `on` say on.
- Any other tool: `--no-<tool>`, e.g. `--no-bash` or `--no-subagent`, the same as `--exclude-tools <tool>`.
- `"oxa": { "webSearch": { "enabled": false } }` or `"provider": "none"` in settings.
- Offline mode (`--offline`, `OXA_OFFLINE=1`, or `oxa.offline` in user settings) removes the tool, and a call that gets through is refused.

A key added or removed during a session takes effect at the next `/reload` or new session, because the tool list is part of the system prompt.

### Use

The model passes `query` (up to 600 characters). Optional parameters are `count` (1 to 20 results), `freshness` (`pd`, `pw`, `pm`, `py`, or a date range `YYYY-MM-DDtoYYYY-MM-DD`; the words `day`, `24h`, `today`, `week`, `month` and `year` are accepted too), `country` (two letters) and `search_lang` (for example `en`).

oxa uses Brave's LLM Context endpoint first. Its output size is `oxa.webSearch.maxTokens` (2048 by default, from 1024 to 32768). If the key's plan does not include that endpoint, oxa switches to Brave's web search endpoint for the rest of the process and says so once.

Each result list starts with the query, then numbered sources in Brave's order, each with a title, URL, date when Brave gives one, and snippets. Long sources are cut, and the list says how many were omitted.

### Permissions and limits

- Workspace mode runs the tool without a prompt. The tool reads and changes nothing on your machine, but the query text leaves it. Plan mode denies the tool, because plan mode has no network. Ask mode asks "Connect to api.search.brave.com" first, and "allow for this session" covers that host.
- Requests take up to 30 seconds. Rate limits and server errors are retried, up to three attempts in all, with backoff.
- Searches run one at a time.
- The key is never written to logs, notices, session files, exports or the model's context. Bash and other child processes do not receive `BRAVE_SEARCH_API_KEY`.
- A key given only in `BRAVE_SEARCH_API_KEY` does not reach a `subagent` child, so the child has no working `web_search`. A key in `brave-search.key` works in children as well.

## codemode, tool_search and call_tool

`codemode` is off by default. When on, the model can run a JavaScript script that calls several tools in parallel and returns only the script's output. The script has no network or file system of its own, and reaches the outside only through tools. Enable it with `codemode.enabled`. An enabled MCP server with codemode exposure turns it on as well.

`tool_search` and `call_tool` are for many tools: the model searches for a tool by name or description, and calls it through `call_tool`. Enable them with `codemode.toolSearch`, or by listing them in `defaultTools`. `codemode.mode: "only"` hides every other tool while codemode is on, so the model reaches tools through scripts only. `codemode.maxWorkers` sets how many scripts run at once; the default is 2.

Details: [extensions.md](extensions.md#codemode-and-tool_search) and [mcp.md](mcp.md#exposure).
