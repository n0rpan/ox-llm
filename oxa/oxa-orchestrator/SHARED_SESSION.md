# Shared local goal control (issue #94, in progress)

The feature-enabled `oxa orchestrate --goal --control ...` command exposes its
existing native goal owner at `<directory>/session.sock`. An external client can
observe and steer that conversation while the foreground command keeps displaying
status. It does not start or restore another inference process when clients attach.
The existing interactive TUI can also expose its actual core session through this
API. Both clients keep read/write access; attachment never starts a second session.

Start with the local-only invocation in [SESSION_GOALS.md](SESSION_GOALS.md), adding
`--control`. The socket appears after native session initialization. It is removed
on orderly owner shutdown. The directory must be a new private directory; an
existing or stale socket is never removed and adopted at startup. Unix socket path
length limits apply. Without `--control`, no attachment listener is created.

## Client requests

The feature-built binary includes an operator client; no Python client or second
inference runtime is needed:

```sh
socket="$HOME/.oxa/agent/project-control/session.sock"
oxa orchestrate-control --socket "$socket" status
oxa orchestrate-control --socket "$socket" events --after 0
oxa orchestrate-control --socket "$socket" request < request.json
```

`request.json` contains one request from the table below, for example
`{"id":"pause-project-1","method":"pause","revision":7}`. Read the current
revision first; this example's revision is not an automatic selection. Use a new
ID for each mutation, read its admission/rejection event, and use the returned
`next` cursor for another events page. The client does not fetch a replacement
revision, retry a mutation, follow events indefinitely, or treat queue acceptance
as admission. Read-only `status`/`events` commands do not read stdin.

Output is one JSON response. Exit 0 means a response without a server error;
`queued: true` still means queued only. Exit 1 means transport/protocol/server
failure, and exit 2 means invalid arguments or input. After an uncertain mutation
response, inspect events before retrying. The client validates the same schema as
the server, bounds requests to 32 KiB and responses to 1 MiB, and checks response
IDs. Connected I/O has a three-second deadline. It requires an explicit socket
path and does not discover settings or start an agent. Operator text is supplied
on stdin rather than command-line arguments.

Connect to the Unix socket and send one UTF-8 JSON object followed by newline. Read
one JSON response followed by EOF. Open another connection for each request.
Multiple clients can read and write concurrently. No writer lease is held by the
foreground command or by an external connection.

```python
import json, socket

def request(path, value):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(3)
        client.connect(path)
        client.sendall(json.dumps(value).encode() + b"\n")
        with client.makefile("rb") as response:
            return json.load(response)

path = "/absolute/home/.oxa/agent/run/session.sock"
status = request(path, {"id": "status-1", "method": "status"})
revision = status["status"]["goal"]["revision"]
print(json.dumps(request(path, {
    "id": "pause-1", "method": "pause", "revision": revision
})))
```

| Method | Fields besides `id`, `method` | Behavior |
| --- | --- | --- |
| `status` | none | Last observed goal state, counters and event sequence; refreshed at least every 250 ms while the owner is running |
| `events` | `after` | Up to 16 events after a sequence; use `next` for the next page, inspect `gap` |
| `pause` | `revision` | Request pause; active work stays visibly pausing until settlement |
| `resume` | `revision` | Continue paused/blocked work if accounting and remaining budget permit |
| `set_goal` | `revision`, `text`, optional `budget` | Set a new TUI goal or redirect settled paused/blocked work; remains paused until explicit resume |
| `steer` | `revision`, `text` | Queue bounded clarification for a running native turn |
| `cancel` | `revision` | Cancel goal work; the TUI retains its session, while the native goal CLI stops/reaps its process |
| `close` | none | End the attachment after `runtime_stopped` is true; rejected while native work remains active |

`id` is a nonempty string of at most 128 bytes without control characters. Unknown
fields and methods are rejected. Use a unique ID for each mutation. Revisions come
from status/events. A mutation's
`queued: true` means it reached the bounded owner queue; it is not confirmation
that it was admitted or consumed by inference. Read the `control_admitted` or
`control_rejected` event carrying the same `request_id`. Two simultaneous mutations
with the same revision can both queue; the owner admits one and rejects the stale
other. On an uncertain response, inspect events/status before deciding to retry.

`set_goal` changes operator instructions in the same session and goal ledger. It
does not reset elapsed time, token usage, turn attempts or output limits, and does
not expand model/tools/filesystem/network permissions. It is rejected during active
or pausing work. Pause first, wait for settlement,
then update and explicitly resume with the new revision. Unknown interrupted usage
continues to block token-bounded resume; changing the goal cannot clear that debt.

## Bounded events and lifecycle

History is memory-only: at most 128 events and 512 KiB, with an explicit `gap` when
the caller missed retained history. Turn replies, including the final DONE reply,
have a UTF-8-safe 16 KiB preview with an explicit `truncated` flag. Full final replies
remain in the foreground report, saved only with `--save-results`. These events are
goal/control/turn events. The TUI attachment also publishes native event names and
text deltas/final replies; it does not copy tool arguments, tool output or hidden
reasoning to the socket.

For the native goal CLI, read `goal_finished` and status `runtime_stopped: true`
for post-cleanup evidence.
The CLI retains the final in-memory events for up to 30 seconds after inference is
stopped/reaped, so polling clients can read the final reply and cleanup event. Send
`close` after collecting them to let the foreground print its final report and
exit immediately; a signal or the 30-second grace period also ends the attachment.
This grace period starts no new inference and does not alter the settled goal's
accounting. Explicit `--save-results` saves the final report before this wait.
The foreground final report remains authoritative for terminal cleanup and outcome.
Socket disconnect does not cancel the goal or clear its context, and reconnect
never resends work. A live control owner can hold paused/blocked work until resume,
cancel or the original deadline. Native process death and cumulative budget stops
retain their existing fail-closed behavior.

The socket is mode 0600 inside an owner-only directory. Access grants local
operator authority; do not expose it through an untrusted directory or proxy.
There is no TCP listener or remote-agent authority path. The server admits at most
eight simultaneous connections, limits a request to 32 KiB, uses a two-second
total connection deadline, and forwards at most 64 queued controls. It never waits
for client I/O while holding the session-state lock. Slow/disconnected clients do
not block native supervision or owned process cleanup. Goal/control payloads are
not persisted by this attachment. The native goal CLI journal remains hash-only;
the normal TUI transcript still follows its existing session persistence policy
(including goal prompts and steering). Use the normal `--no-session` option when
you want an ephemeral TUI transcript.

The real RPC fixture verifies two simultaneous external writers, stale revision
rejection with request correlation, paused goal redirection with no budget reset,
retained context and one native process. The CLI fixture independently attaches,
updates/resumes or cancels blocked work, checks the final report and reaped process,
and verifies socket removal and opt-in-only result persistence. All inference is
owned loopback fake traffic.

## Existing TUI attachment

Build the feature-enabled binary, then explicitly enable attachment for one TUI:

```sh
cargo build --locked -p oxa-cli --features orchestrator --bin oxa
OXA_SESSION_CONTROL_DIRECTORY=/absolute/home/.oxa/agent/new-control-directory \
OXA_ENGINE_URL=http://127.0.0.1:PORT \
./target/debug/oxa --offline --model MODEL
```

Use only a verified local/nonbillable route for the brief's model-free workflow.
This example is not permission to make a cloud request. The control directory must
be absolute, new, outside the agent workspace, and under the session
`agent_dir` (normally `$HOME/.oxa/agent`). This is the existing deny-list location
for both file tools and sandbox mounts; a merely private directory elsewhere can
remain reachable by a long-lived command. Use an owner-controlled parent that
already exists. Symlink aliases of the agent directory are rejected; use its
canonical path when overriding `OXA_AGENT_DIR`. The adapter rejects a workspace-exposed directory
before creating it. A session swap that exposes the directory disables attachment.
No setting file is changed. The feature and environment switch are both required;
ordinary builds and unset environments keep the existing TUI behavior.

The socket's `status.session.id` is the ID of the session the TUI worker actually
owns. `session.busy`, `session.model` and `session.mode` report the existing state and
configuration; the API
cannot change model, tools, permissions, endpoint, sandbox or offline policy.
The worker still owns all prompts, tools, dialogs and session changes. Socket
steering/abort uses its existing `Handle`, and goal continuations run on that same
worker between TUI commands. Reply text and native events reach the original TUI
and external observers. There is no RPC subprocess or replacement terminal UI.

Initially no goal is set (`reason: no_goal`). Use the current revision to submit:

```json
{"id":"project-goal","method":"set_goal","revision":1,"text":"Review the selected project lane and verify the fix","budget":{"wall_ms":3600000,"tokens":null,"turns":128}}
```

Wait for its admission event, then `resume` with the new revision. A new TUI goal
may specify a budget (default: one hour, 128 command/goal-turn attempts, time-only
usage accounting). Pauses count against time. An explicit new goal is allowed after
DONE, cancellation or exhaustion and starts a new ledger in the SAME conversation.
Redirecting a paused/blocked goal keeps its ledger and rejects a replacement
budget. The native goal CLI's initial ledger is immutable and rejects `budget` on
`set_goal`; it cannot be reset through this API.

Goal state, attempts and usage appear as notices in the existing TUI. Normal TUI
commands during active goal work share its cumulative budget. A normal reply does
not complete the goal: the next continuation requests the structured declaration
again. Paused/blocked goals reject new TUI command work until resume or cancel;
viewing history and changing sessions remain available. An exhausted ledger also
rejects new TUI work until an explicitly funded new goal or a session change.
TUI Esc pauses autonomous
progression after the running turn settles. An interrupted turn's uncertain usage
stays unknown; token-bounded goals cannot resume it with a fabricated zero.

This bounded goal profile disables automatic agent retry while a goal is
active/paused/blocked and restores the previous switches afterward. Automatic
compaction stays off unless the operator enables the engine-metered
[main-session compaction opt-in](MAIN_USAGE.md). It suppresses
unrelated idle-host work and invisible draft/prefix requests while a goal ledger
owns the session, including after exhaustion. This prevents hidden follow-on
inference from bypassing a stop. Long cloud compaction/reliability is still a
separate acceptance item. Time and observed token limits request abort through the
same core handle; they are not a provider-side monetary cap or evidence of stopped
cloud billing. No cloud test has been run.

DONE ends goal work but keeps the TUI session and in-memory events available. A
settled goal has `turn_active: false`; `runtime_stopped` stays false because the TUI
owner is still alive, so socket `close` cannot close the TUI. Exit the terminal to
stop its owned work and remove the socket. `/new`, resume and fork rebind socket
identity and advance the attachment revision; stale controls cannot start work on
the replacement session. A new session has no inherited goal or budget debt.

The hooks are limited to `oxa-interactive/src/app.rs`: attachment lifetime, session
rebinding, command admission/settlement, idle progression, draft gating and event
forwarding. All are behind the default-off `session-control` feature; the adapter
and socket implementation live in `oxa-orchestrator`. `oxa-cli`'s existing
`orchestrator` feature opts into these hooks. No core/config/sandbox code changes.

`shared_tui` runs the actual `run_session` with a scripted terminal and concurrent
socket clients against owned loopback fakes. The shared-session scenarios verify ordinary
TUI turn revisions and stale steering, private attachment placement, shared identity/context,
TUI and external steering, multi-turn completion, live pause/resume and goal
redirection without accounting resets, unknown interrupted usage, TUI Esc,
`/new` rebinding/stale rejection, time/token stops, cancellation, and in-flight TUI
exit cleanup. The normal feature-enabled TUI regression suite also runs without
the environment switch to verify ordinary behavior stays inert. Opt-in [background commands and wake-ups](BACKGROUND_TASKS.md) now use that same
owner. Worker-lane integration is the next priority; full-evening cloud
acceptance and coordinator/independent review gates remain open.

## Client connection refusal and reconnect acceptance

Control connection admission is nonblocking. If the explicit Unix listener has
a full backlog, the client returns a transport failure instead of waiting for
space before its connected-I/O deadline can begin. It does not start another
runtime, search for another socket or resend a mutation. The owned descriptor
is close-on-exec and is closed on connection failure. Connected reads retain
their absolute deadline, response bound and correlation checks. A kernel/filesystem
stall is not claimed to have a hard elapsed-time bound.

The backlog regression uses a real owned Unix listener, fills its accept queue,
and proves the old blocking connection misses the test deadline. Cleanup always
releases the listener and joins the test thread, including on failure. The
rebuilt real CLI also exits with transport failure against that full backlog.

The actual CLI/tmux demo now deliberately sends one complete steering mutation
and disconnects without reading the response. It reconnects through
`oxa orchestrate-control events`, resumes from the captured sequence, checks
there is no history gap, and finds exactly one matching `control_admitted`
event. The control CLI then observes the original session. The next inference
request contains the clarification exactly once, the same parent goal loop
continues, and the TUI exits normally. Demo mutations use unique request IDs.
No uncertain mutation is replayed and no second inference session is started.

This is owned-loopback mock evidence for reconnect mechanics. It does not prove
recovery after history eviction or owner replacement, durable request-ID storage,
exactly-once inference, or live-provider working-evening acceptance. A history
gap or missing admission event remains uncertainty and must not authorize blind
mutation replay.
