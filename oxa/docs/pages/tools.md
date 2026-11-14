# Tools

The model acts through tools. oxa has seven built-in tools, which are on by default, and a few optional ones: `subagent`, `web_search`, `codemode`, `tool_search` and `call_tool`. Extensions and MCP servers add more. Every tool result is capped before it reaches the model.

## The built-in tools

| Tool | Access | What it does |
|---|---|---|
| `read` | read | reads a file. Parameters `path`, `offset` (first line, 1-based), `limit` (lines). A call returns at most 2000 lines or 16 KB; continue with `offset` for the rest. Under profile `local` a call with neither `offset` nor `limit` returns a shorter first page; see [profile.md](profile.md) |
| `bash` | exec | runs a shell command in the project in the sandbox. Parameters `command`, `timeout` (seconds). With no `timeout` there is no limit under profile `pi`, and 120 seconds under `local`; `oxa.bash.defaultTimeout` sets it. Output over 16 KB is cut to its first and last parts, and the full output is saved to a file the model can read |
| `edit` | write | replaces exact text in one file. Under `pi` takes `edits` (a list of `oldText` / `newText`); under `local` takes one `oldText` / `newText` pair per call. See [profile.md](profile.md) |
| `write` | write | writes a file, creating it and its parent directories, and overwrites an existing file |
| `grep` | read | searches file contents for `pattern`. Optional `path`, `glob`, `ignoreCase`, `literal`, `context` and `limit` (default 100 matches). Respects `.gitignore`; lines are cut at 500 characters. Under profile `local`, when matches exceed `limit`, the answer is the total and one count per file instead of the lines; see [profile.md](profile.md) |
| `find` | read | finds files by glob `pattern` under `path`, up to `limit` results (default 1000). Respects `.gitignore` |
| `ls` | read | lists a directory, sorted, directories with a trailing `/`, dotfiles included. Up to `limit` entries (default 500) |

The access class decides what permission mode asks for; see [security.md](security.md#permission-modes). Paths are resolved against the project directory, and `..` and symbolic links are followed before the check.

Tool names, descriptions and schemas are part of the system prompt. They stay the same during a session, and change only after `/reload`, a model switch or a new session.

### Output limits

- Any tool output sent to the model is capped at `oxa.toolOutputCap` bytes (16384 by default). Output above the cap is cut and says so. Under profile `local`, when `oxa.compaction.softTokens` is set or the engine publishes a compaction point, one tool result is at most a quarter of it (three bytes per token, never below 2048 bytes), unless `oxa.toolOutputCap` is set. Only the first page of a `read` (no `offset` or `limit`) is at most 8 KiB; `grep`, `find`, `ls` and a `read` with `offset` or `limit` are cut at the cap itself, with a note inside the cap that says how to go on. See [profile.md](profile.md).
- `read`, `grep`, `find` and `ls` apply their own line or match limits first.
- For bash, the full output is kept in a file, and the model is told where it is.

## Choosing tools

| Option or setting | Effect |
|---|---|
| `--tools a,b` (`-t`) | allowlist of names or patterns; `*` matches any characters. Only these tools are on |
| `--exclude-tools a,b` (`-xt`) | denylist, applied after the allowlist |
| `--no-builtin-tools` (`-nbt`) | disables the built-in tools; extension and MCP tools stay |
| `--no-tools` (`-nt`) | disables all tools |
| `--no-mcp` | no MCP servers connect and no MCP tools are offered |
| `defaultTools` setting | the list of tools on by default. Names may include `subagent`, `web_search`, `codemode` and `tool_search`, and a list can be patched with `+name` and `-name` |

Examples:

```sh
oxa -p --tools read,ls "List the files"
```

```console
$ oxa --tools 'read,grep,find,ls' --permission-mode ask
$ oxa --exclude-tools bash
```

A read-only session: `oxa --tools read,grep,find,ls`. In plan mode the write tools are refused, and bash only runs read-only commands with no network.

## Repair turn

Some models write a tool call as text instead of a structured call, or write a malformed one. When a reply has no tool call but its text contains a tool-call opener (`<tool_call>` or `<function=`) outside a code block, oxa:

1. classifies the first such block: an unknown function (with the list of available tools), an unknown parameter (with the valid names), a missing required parameter, an unclosed block, invalid JSON, a wrong type, or a bad format;
2. appends one fixed user message that names the problem and asks for a corrected call. The message is stored as a hidden `custom_message` entry;
3. runs the next turn.

At most two repairs can follow each other. A valid tool call resets the count. The third malformed reply ends the run; the raw reply stays visible, with a stop message. Each repair shows a warning notice and adds to the footer count `repairs N`.

`oxa.toolRepair: false` turns repairs off, and a malformed reply then ends the run at once.

## Repeated calls

A model can get stuck making the same tool call again and again. When a call (any tool: `read`, `bash`, `grep`, `find`, `ls`, `web_search`, MCP and extension tools) has returned the same result three times in a row, with the same arguments and nothing else in between, oxa still runs it but sends a short note instead of the result: the result is unchanged and already in the conversation, change approach. Failing results count too. The second note is the last: the next identical call ends the run with a stop message. A re-run after an edit or any other call, a command whose output changed (a test after a fix, a polling command), or the same call after a new prompt is never affected.

`oxa.toolRepeatGuard: false` turns this off.

## subagent

`subagent` lets the model hand a task to a child oxa that works in its own context and returns only its final answer. It keeps long searches out of the main conversation.

- Shapes: `{agent, task}` for one child; `{tasks: [...]}` for several, run one after another in order; `{chain: [...]}` for steps where each step can use `{previous}`, the output of the step before.
- Agents are Markdown files in `~/.oxa/agent/agents/` and, in a trusted project, `.oxa/agents/`. The file has a `name` and a `description` in its front matter, and its body is the child's system prompt. A project agent with the same name replaces the user agent. A `model:` in an agent file is ignored with a warning; the child uses the session's model.
- The child is a new oxa process in JSON print mode, with the same model, permission mode and tools, and no session file. It runs on its own compaction limit ([compaction.md](compaction.md)).
- One child runs at a time. A call with more than 8 tasks is refused.
- The result is the child's final text, capped at `oxa.subagent.resultCap` (6144 bytes). A longer result is cut and the full text is saved to a file under the agent directory's `tmp/`.
- Interrupting the parent stops the child: SIGTERM to its process group, then SIGKILL after 2 seconds.
- Plan mode refuses the tool, because it runs a process.

The tool is on by default when the session's model is not Flash and its context window is at least 32K. `oxa.subagent.enabled` sets it explicitly, and `oxa.subagent.smallContext: true` allows it on a smaller window. Children do not get `subagent` themselves.

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
- `--no-web-search`, or `OXA_WEB_SEARCH=0` (also `false`, `no` or `off`).
- `"oxa": { "webSearch": { "enabled": false } }` or `"provider": "none"` in settings.
- Offline mode (`--offline`, `OXA_OFFLINE=1`, or `oxa.offline` in user settings) removes the tool, and a call that gets through is refused.

A key added or removed during a session takes effect at the next `/reload` or new session, because the tool list is part of the system prompt.

### Use

The model passes `query` (up to 600 characters). Optional parameters are `count` (1 to 20 results), `freshness` (`pd`, `pw`, `pm`, `py`, or a date range `YYYY-MM-DDtoYYYY-MM-DD`), `country` (two letters) and `search_lang` (for example `en`).

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
