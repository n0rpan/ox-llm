# Output modes and RPC

oxa has three non-interactive ways to run: plain print, a JSON event stream, and an RPC mode where another program drives a session over stdin and stdout. They use the same events as the interface, so a client sees what a person would see.

| Mode | Started with | Input | Output |
|---|---|---|---|
| print | `-p` (text is the default `--mode`) | the message, `@files`, and standard input | the final answer on stdout |
| JSON | `--mode json -p` | the same | one JSON object per event on stdout |
| RPC | `--mode rpc` | JSON commands, one per line, on stdin | responses and events, one JSON object per line, on stdout |

Print mode and JSON mode run to the end of the prompts and exit. RPC mode runs until the client closes standard input.

Standard output carries only records in JSON and RPC modes. Diagnostics go to standard error. Exit codes are those of [cli.md](cli.md#commands).

## Print mode

```sh
oxa -p "Say hello"
```

The reply is written to standard output, and the process exits 0 when the run finished. Standard input is read as well when it is not a terminal, and it becomes the start of the first prompt, followed by the message:

```console
$ cat error.log | oxa -p "Explain this error"
```

`@file` arguments are attached to the first prompt, and each further message is a separate prompt run in order:

```console
$ oxa -p @src/main.rs "Add a flag for verbose output" "Now add a test for it"
```

Print mode does not ask questions. A permission question is answered "deny" unless the mode allows the action, and project resources that need approval are not loaded, with a notice ([security.md](security.md#project-trust)).

## JSON mode

```sh
oxa --mode json -p "hi"
```

The first line is the session header:

```json
{"type":"session","version":3,"id":"...","timestamp":"...","cwd":"/home/me/proj"}
```

Each later line is one event, with a `type` field. The events are, in the order they happen within a run:

| Group | Event types |
|---|---|
| run | `agent_start`, `turn_start`, `turn_end`, `agent_end`, `agent_settled` |
| messages | `message_start`, `message_update`, `message_end`, with deltas such as `text_delta`, `thinking_delta` and `toolcall_delta` |
| tools | `tool_execution_start`, `tool_execution_update`, `tool_execution_end`, and `tool_repair` for a repaired call |
| context | `compaction_start`, `compaction_end`, `entry_appended`, `prefix_epoch`, `cache_miss`, `prefill`, `engine_queue` |
| retries | `auto_retry_start`, `auto_retry_end`, `summarization_retry_scheduled`, `summarization_retry_attempt_start`, `summarization_retry_finished` |
| state | `queue_update`, `model_select`, `thinking_level_changed`, `session_info_changed`, `mode_changed`, `bash_execution_update` |
| other | `notice` (a message for the user, with a level), `extension_error` |

A `cache_miss` event carries the expected and the cached token counts and the cause (see [compaction.md](compaction.md#cache-misses)). A `notice` carries the text shown in the interface. `agent_settled` is the last event of a run.

The message events carry the message as a JSON object with `role` and `content`. Assistant text is a list of blocks of type `text` and `thinking`, and tool calls are `toolCall` blocks with an `id`, a `name` and `arguments`.

## RPC mode

```console
$ oxa --mode rpc
```

The client writes one command per line to standard input. Each line is a JSON object with a `type` field, and an optional `id` that the response repeats:

```json
{"id":"1","type":"get_state"}
{"id":"2","type":"prompt","message":"List the files"}
```

Every command gets one response line:

```json
{"type":"response","command":"get_state","success":true,"id":"1","data":{...}}
{"type":"response","command":"set_model","success":false,"error":"Model not found: ...","id":"3"}
```

Between responses the process writes the events of a run, in the same shape as JSON mode. Lines are separated by LF only.

### Commands

Commands that start or steer a run:

| Command | Fields | Effect |
|---|---|---|
| `prompt` | `message`, `streamingBehavior` (optional) | start a run with the message. While a run is going, the prompt is refused unless `streamingBehavior` is `steer` or `followUp`, which queues it |
| `steer` | `message` | queue a steering message for the running turn |
| `follow_up` | `message` | queue a message to run when the current run finishes |
| `abort` | | stop the running turn |
| `abort_retry` | | stop a retry that is waiting |
| `clear_queue` | | drop queued messages |
| `set_steering_mode`, `set_follow_up_mode` | `mode` (`all` or `one-at-a-time`) | how queued messages are sent |
| `set_auto_compaction`, `set_auto_retry` | `enabled` | switch automatic compaction or retries for this process |
| `set_mode` | `mode` (`plan`, `workspace` or `ask`) | permission mode |

Commands about the state:

| Command | Result |
|---|---|
| `get_state` | model, thinking level, permission mode, session id, name and file, whether a run or a compaction is going, and the queue sizes |
| `get_messages` | the messages of the active branch |
| `get_session_stats` | token totals, requests, cache counts, tool repairs |
| `get_available_models` | the models the engine serves |
| `set_model` | `modelId`: switch model (refused, with `Model not found`, for an unknown id) |
| `cycle_model` | the next model in the scoped cycle |
| `set_thinking_level`, `cycle_thinking_level`, `get_available_thinking_levels` | `level`: thinking level |

Commands about the session:

| Command | Fields | Effect |
|---|---|---|
| `new_session` | | start a new session |
| `compact` | `customInstructions` (optional) | compact now; input queued during the compaction runs afterwards |
| `bash` | `command`, `excludeFromContext` (optional) | run a shell command as `!cmd` does |
| `abort_bash` | | stop a running `bash` command |
| `get_entries`, `get_tree`, `get_fork_messages`, `get_last_assistant_text` | | the session's entries, tree, forkable messages, last reply |
| `fork` | `entryId` | fork at an entry from `get_fork_messages` |
| `clone` | | duplicate the session at the current point |
| `switch_session` | `sessionPath` | open another session file |
| `set_session_name` | `name` | name the session |
| `export_html` | `outputPath` (optional) | write the session as HTML; returns the path |
| `get_commands` | | the slash commands and skills available |

`extension_ui_response` answers a dialog from an extension; see below.

### Permission questions and extension dialogs

Extensions and the permission gate ask the user through dialogs. In RPC mode each dialog becomes an `extension_ui_request` event that carries an `id` and a `method`: `select`, `confirm`, `input`, `editor` or `notify`, plus the setters (`setStatus`, `setWidget`, and so on, which need no answer). A dialog that needs an answer waits until the client sends:

```json
{"type":"extension_ui_response","id":"<the request id>","value":true}
```

The value is the chosen option for `select`, a boolean for `confirm`, and the text for `input` and `editor`. A permission question is a `confirm` request, and its title says what is asked: `read <path>`, `write <path>`, or ``run `<command>` with network access``. Answer `true` to allow once.

A dialog that gets no answer in time is dismissed, and the request is treated as having no answer; a permission question is then denied. Dialogs from extensions have an optional `timeout` in milliseconds.

## The Rust client

The `oxa-rpc` crate has a client for driving `oxa --mode rpc` from Rust. It starts the process, sends commands, and reads the events. The main functions are `RpcClient::spawn` (start the process with its arguments), `request` (send a command and wait for its response, with a timeout), `next_event` and `wait_for(kind)` (read event records), `respond_ui` (answer an `extension_ui_request`) and `close`. The crate is not published; add it by path from a checkout of the source tree.

## Compatibility

The record shapes follow the earlier agent's JSON and RPC formats field for field. oxa adds events and commands (`set_mode`, `tool_repair`, `cache_miss`), and clients should ignore fields and events they do not know.
