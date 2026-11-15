# Extensions

An extension is a JavaScript file that adds to oxa: it can listen to events, add tools and slash commands, show interface elements, and transform what the model sees. Extensions run in an embedded QuickJS runtime. They have no file, network or process access of their own; the calls they make go through oxa's policy and the sandbox. For the full type declarations, read [oxa-ext/oxa.d.ts](../../oxa-ext/oxa.d.ts) in the source tree.

Two more mechanisms need no code: **exec hooks**, which run a command on an event, and **skills** and **prompt templates**, which are text ([skills.md](skills.md)).

## Files and loading

An extension is an ES module with a default export, a function that receives the `oxa` API:

```js
export default function (oxa) {
  oxa.on("tool_call", (event) => {
    if (event.toolName === "bash" && /rm -rf \//.test(event.input.command)) {
      return { block: true, reason: "refusing a root delete" };
    }
  });
}
```

The file must end in `.js` or `.mjs`. A TypeScript file (`.ts`) is rejected with a message that says to write JavaScript. The factory may be `async`.

oxa loads extensions from these places, in order:

1. files and directories directly inside `~/.oxa/agent/extensions/`. A directory is loaded through its `index.js` (or `index.mjs`);
2. the project's `.oxa/extensions/`, in a trusted project only;
3. the paths in the `extensions` setting; `~` is expanded, and relative paths are relative to the agent directory;
4. the paths given with `-e` or `--extension` (repeatable), relative to the current directory. `-e builtin:<name>` loads a built-in extension.
5. installed packages ([Packages](#packages)).

`--no-extensions` (`-ne`) disables discovery and the built-in extensions; `-e` paths still load. A file loaded from the project is content-approved like the rest of the project ([security.md](security.md#project-trust)). `/reload` loads changes; a loaded extension that fails is reported and does not stop the others.

## The API

The `oxa` object provides these functions. The names follow the common extension API of the earlier agent, with `oxa` as the global object.

| Area | Functions |
|---|---|
| events | `oxa.on(event, handler)` returns a disposable |
| tools | `registerTool({ name, description, parameters, class, exposure, execute })`. `parameters` is a JSON Schema object; `class` is `read`, `write`, `exec` or `net` and decides the permission prompt (default `exec`); `exposure` is `direct`, `model-only`, `codemode`, `deferred` or `hidden` |
| commands | `registerCommand(name, { description, handler(args, ctx), getArgumentCompletions })` |
| keys | `registerShortcut(key, { description, handler(ctx) })`; a built-in binding wins over an extension shortcut |
| flags | `registerFlag(name, { type: "boolean" or "string", default })` and `oxa.getFlag(name)` |
| messages | `sendMessage(message, { triggerTurn, deliverAs })`, `sendUserMessage(content, { deliverAs })`, `appendEntry(customType, data)` |
| session | `setSessionName`, `getSessionName`, `setLabel`, `setModel`, `getThinkingLevel`, `setThinkingLevel`, `getActiveTools`, `setActiveTools`, `getAllTools`, `getSettings`, `getCommands`, `addPromptSection` |
| processes | `oxa.exec(command, args, { cwd, timeout, net })` runs a program under the session's policy and sandbox |
| rendering | `registerMessageRenderer`, `registerEntryRenderer`, `registerToolRenderer`, `registerMarkdownTransformer` |

The handler's `ctx` provides:

- `ctx.ui`: `select`, `confirm`, `input`, `editor` (dialogs that wait for an answer), `notify`, `setStatus`, `setWidget` (above or below the editor), `setFooter`, `setHeader`, `setTitle`, `setWorkingMessage`, `setEditorText`, `getEditorText`, `addAutocompleteProvider`, and `setEditorComponent` for a custom component;
- `ctx.hasUI` and `ctx.mode`: whether a user can answer (`true` in the interface and RPC modes), and the run mode;
- `ctx.cwd`, `ctx.sessionManager` (read-only view of the session), `ctx.thinkingLevel`, `ctx.model`;
- `ctx.fs.readFile(path)`, `ctx.fetch(url, init)`: file and HTTP access through the policy;
- `ctx.executeTool(name, args)`, `ctx.getContextUsage()`, `ctx.isIdle()`, `ctx.abort()`, `ctx.compact(options)`, `ctx.getSystemPrompt()`.

A command handler also has `ctx.waitForIdle()`, `ctx.fork(entryId)`, `ctx.navigateTree(target, options)` and `ctx.reload()`.

Without a user (print mode, `hasUI: false`), dialogs answer `null` at once, so an extension should not depend on them.

Some functions of the earlier extension API are not in oxa. They are:

- `registerProvider`, `unregisterProvider`, `registerVirtualModel` and `unregisterVirtualModel`: model providers and virtual models are not registered by extensions;
- `registerMcpServer`, `unregisterMcpServer` and `getMcpServers`: MCP servers come from `mcp.json` ([mcp.md](mcp.md));
- `ctx.newSession` and `ctx.switchSession`: the user starts or switches sessions with `/new` and `/resume`;
- the `before_provider_headers` event;
- Node and npm imports.

Calling one of them throws an error with the code `unsupported_in_oxa`. The full declarations are in [oxa-ext/oxa.d.ts](../../oxa-ext/oxa.d.ts).

## Events

Handlers run in order of registration. Events that can change the outcome return a value:

| Event | Can return |
|---|---|
| `tool_call` | `{ block, reason, input }`: block the call, or change its arguments |
| `tool_result` | a changed result |
| `input` | `{ action: "transform" \| "handled" \| "continue", text }` for what you type |
| `context`, `context_with_system`, `before_provider_request` | `{ messages }`: a changed message list |
| `project_trust` | `{ trusted: "yes" \| "no" \| "undecided", remember }` |
| `session_before_compact` | a custom summary, used as the compaction summary |

The other events observe: `session_start`, `session_shutdown`, `session_before_switch`, `session_before_fork`, `session_compact`, `session_compact_failed`, `session_before_tree`, `session_tree`, `before_agent_start`, `agent_start`, `agent_end`, `agent_settled`, `turn_start`, `turn_end`, `message_start`, `message_update`, `message_end`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`, `model_select`, `thinking_level_select`, `user_bash`, `ui_prompt_start`, `ui_prompt_end`, `resources_discover`, `cache_warming_decision` and `after_provider_response`.

`session_compact` fires after a compaction, and `session_compact_failed` when a compaction failed and the conversation was left as it was. A failed summary does not stop the compaction: a mechanical record replaces it ([compaction.md](compaction.md)). The compaction then happens, so `session_compact` fires, not `session_compact_failed`, and its event has `mechanical: true` and `summaryError`, the reason the summary failed. An extension that must know when the model summary failed checks both.

Changes to the message list are checked against the prompt cache. A `context` result that changes the start of the conversation is refused, because it would invalidate the whole cache. An extension that must rewrite history declares `rebase: true`; oxa then accepts the change and reports the cache miss with the cause `extension` ([compaction.md](compaction.md#cache-misses)).

## Exec hooks

Exec hooks need no JavaScript. The `hooks` setting maps an event name to a command:

```json
{
  "hooks": {
    "tool_call": { "command": "/home/me/bin/check-call", "timeout": 10 },
    "session_start": "/home/me/bin/greet"
  }
}
```

The event names are the names in the list above, such as `tool_call`, `session_start` and `agent_settled`. An entry is a command string, or an object with `command` and `timeout` (seconds; default 30), or a list of those.

- The command runs as `bash -c` in the sandbox, in the session's directory. It can write inside the project. It has no network.
- It receives one JSON line on standard input: `{"event": ..., "cwd": ..., "payload": ...}`.
- Exit 0 with no output continues. Exit 0 with a JSON object on standard output is a reply in the same shape as the handler results above. Exit 2 blocks the `tool_call` or `user_bash` event, or cancels a `session_before_*` event, and its standard output is the reason. Any other exit code, a timeout, or output that is not JSON continues, with a warning.

Hooks run on every such event, without the model asking. Keep them short, and read them before you trust a project that has them.

## Packages

A package bundles extensions, skills, prompt templates and themes in one directory. Its `package.json` can name those paths under an `oxa` key; without the key the conventional directories are used. It is installed from a git URL or a local path:

```console
$ oxa install https://example.com/team/oxa-tools.git
$ oxa install ../my-oxa-package
$ oxa install -l ../my-oxa-package
$ oxa list
$ oxa update
$ oxa remove https://example.com/team/oxa-tools.git
```

`-l` writes the source to the project's settings instead of the user's. npm sources are not supported. A git package is cloned with hooks disabled. `oxa update` fetches the newest version of each installed package, or of the one you name. The `packages` setting holds the list; `oxa install` and `oxa remove` edit it.

## Examples

`examples/extensions/` in the source tree has twelve ports of the example extensions of the earlier agent, with a short `README.md` that explains each one:

| Directory | Shows |
|---|---|
| `plan-mode/` | a read-only planning mode: tool sets switched with `setActiveTools`, a status line, a shortcut and a command |
| `permission-gate/` | blocking tool calls on `tool_call`, with a `select` dialog |
| `protected-paths/` | refusing tool calls that touch chosen paths, with a notice |
| `git-checkpoint/` | checkpoints the working tree with `oxa.exec` and git around turns, and offers a restore on fork |
| `custom-compaction/` | supplying the summary on `session_before_compact`, and a command |
| `trigger-compact/` | compaction triggered from `turn_end` and from a command |
| `tools/` | a command that switches tool sets, with a custom component |
| `commands/` | slash commands with `select` and `confirm` dialogs |
| `input-transform/` | rewriting what you type on the `input` event |
| `question/` | a tool that asks the user a question in a custom component |
| `status-line/` | footer status updated on turn events |
| `todo/` | a todo tool and command whose state is kept in the session |

Each directory has an `index.js` and a `session.fake.json` script for the fake engine that the tests use. Copy one as a starting point.

## codemode and tool_search

Extensions can register tools, and a tool can be exposed in three ways. `direct` tools are declared to the model with every request. `codemode` tools are callable only from a codemode script. `deferred` tools are found with `tool_search` and called through `call_tool`. The last two keep the tool list short, which keeps the cache valid when tools come and go. Settings and behaviour are in [tools.md](tools.md#codemode-tool_search-and-call_tool).
