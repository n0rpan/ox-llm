# Shared local goal control (issue #94, in progress)

The feature-enabled `oxa orchestrate --goal --control ...` command exposes its
existing native goal owner at `<directory>/session.sock`. An external client can
observe and steer that conversation while the foreground command keeps displaying
status. It does not start or restore another inference process when clients attach.
The existing interactive TUI is not attached yet; its adapter is the next step in
this branch. This is not completion of priority 2 in #94.

Start with the local-only invocation in [SESSION_GOALS.md](SESSION_GOALS.md), adding
`--control`. The socket appears after native session initialization. It is removed
on orderly owner shutdown. The directory must be a new private directory; an
existing or stale socket is never removed and adopted at startup. Unix socket path
length limits apply. Without `--control`, no attachment listener is created.

## Client requests

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

path = "/absolute/private/run/session.sock"
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
| `set_goal` | `revision`, `text` | Redirect settled paused/blocked work; remains paused until explicit resume |
| `steer` | `revision`, `text` | Queue bounded clarification for a running native turn |
| `cancel` | `revision` | Cancel the goal and stop/reap its owned native process |
| `close` | none | End the attachment after `runtime_stopped` is true; rejected while native work remains active |

`id` is a nonempty string of at most 128 bytes without control characters. Unknown
fields and methods are rejected. Revisions come from status/events. A mutation's
`queued: true` means it reached the bounded owner queue; it is not confirmation
that it was admitted or consumed by inference. Read the `control_admitted` or
`control_rejected` event carrying the same `request_id`. Two simultaneous mutations
with the same revision can both queue; the owner admits one and rejects the stale
other. On an uncertain response, inspect events/status before deciding to retry.

`set_goal` changes operator instructions in the same session and goal ledger. It
does not reset elapsed time, token usage, turn attempts or output limits, and does
not expand model/tools/filesystem/network permissions. It is rejected during active
or pausing work and after terminal completion. Pause first, wait for settlement,
then update and explicitly resume with the new revision. Unknown interrupted usage
continues to block token-bounded resume; changing the goal cannot clear that debt.

## Bounded events and lifecycle

History is memory-only: at most 128 events and 512 KiB, with an explicit `gap` when
the caller missed retained history. Turn replies, including the final DONE reply,
have a UTF-8-safe 16 KiB preview with an explicit `truncated` flag. Full final replies
remain in the foreground report, saved only with `--save-results`. These events are
goal/control/turn events, not yet incremental native token/tool events.

Read `goal_finished` and status `runtime_stopped: true` for post-cleanup evidence.
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
not persisted by this attachment; the existing goal journal remains hash-only.

The real RPC fixture verifies two simultaneous external writers, stale revision
rejection with request correlation, paused goal redirection with no budget reset,
retained context and one native process. The CLI fixture independently attaches,
updates/resumes or cancels blocked work, checks the final report and reaped process,
and verifies socket removal and opt-in-only result persistence. All inference is
owned loopback fake traffic.

## Remaining TUI integration

`oxa-interactive::worker_loop_with` owns the actual `oxa_core::Session`, while
the terminal holds its cloneable `Handle` and sends `SessionCmd`s. The adapter must
use that owner and handle: it must not spawn a second RPC session or build a
replacement terminal application. Existing prompt/tool events and dialogs must
continue to reach the TUI while external observers receive the same session's
events. Session replacement must rebind the attachment identity and reject stale
controls. Goal accounting must cover that owner's turns, including TUI input.

Implementation belongs in the new crate with only small, default-off hooks at
worker initialization, command/turn boundaries and event forwarding. A running
turn cannot wait for the worker's next command dequeue before admitting steering
or requesting abort. The cross-thread broker therefore needs the existing
`Handle`, with explicit acknowledgement of settlement before showing paused.
Goal progression remains with the session owner, serialized with TUI commands.

The integration test must run the existing scripted-terminal TUI and an external
socket client concurrently against one fake-backed core session, verify shared
identity/context, observe replies in both clients, steer and pause an active turn,
redirect/resume the goal without accounting resets, and exercise session swaps
and TUI exit cleanup. The current socket/CLI fixtures are prerequisites, not
evidence that this TUI acceptance has passed.
