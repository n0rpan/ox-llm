# Background commands in the shared TUI session (#94)

The project orchestrator can start a test run or CI watch through
`background_command`, continue its conversation, and receive a completion wake-up
in the SAME session. Commands use the existing core permission policy and sandbox;
this feature does not enable a command or network grant that the session lacks.

Build with `--features orchestrator` and enable both explicit environment switches:

```sh
OXA_SESSION_CONTROL_DIRECTORY=/absolute/home/.oxa/agent/new-control-directory \
OXA_BACKGROUND_TASKS=1 \
OXA_ENGINE_URL=http://127.0.0.1:PORT \
./target/debug/oxa --offline --model MODEL
```

Use a verified local/nonbillable route for the brief's offline workflow. This is
not authorization for a cloud run. The control directory must be under the session
agent directory (normally `$HOME/.oxa/agent`), which the normal file-tool guard
and sandbox deny-list hide. Other control directory rules in
[SHARED_SESSION.md](SHARED_SESSION.md) apply. The feature is off without the build
feature or either environment switch. It currently belongs to the existing TUI
owner; the bounded native RPC worker profile does not inherit these switches.

## Model tool

```json
{"operation":"start","command":"cargo test --locked -p project-package","timeout_s":120,"output_bytes":65536,"wake":true}
```

The result is an admitted task ID with `state: starting`, not successful execution.
The owned runner then performs the sandbox's normal refusal/preparation checks and
starts the command. Preparation/spawn errors become failed task results. The tool
returns promptly, so other model/tool work can continue while the command runs.

| Operation | Fields | Result |
| --- | --- | --- |
| `start` | `command`; optional `timeout_s`, `output_bytes`, `wake` | A new admitted task |
| `list` | none | Bounded task summaries without raw output |
| `status` | `task_id` | Full bounded output and current state for this session's task |
| `cancel` | `task_id` | Requests cancellation; wait for `process_stopped: true` |
| `forget` | `task_id` | Removes a settled result after its result was read/delivered |

Argument objects reject unknown fields. Mode, writable paths, network access and
sandbox arguments are not tool parameters. The tool is a default-selection member
only in this opted-in profile; `--no-tools`, exclusions and explicit allowlists
still control its availability. Every start goes through the ordinary `Access::Exec`
policy, including plan/ask modes, denials and approval dialogs. Shell path/prefix,
the tool environment and sandbox layout come from the same session as normal bash.

States are starting, running, canceling, completed, failed or canceled. Exit 0 is
completed; nonzero exit, signal, output limit, timeout and preparation errors are
not success. `process_stopped: true` is published only after the owned runner has
reaped its process (or proved it never spawned). A cancellation acknowledgement
does not claim cleanup is finished. IDs include a fresh 128-bit owner nonce and a
monotonic counter; forgetting, reopening or restarting cannot reuse a stale ID.
Tasks and results are session-scoped and cannot be looked up through another
session's tool context.

## Wake-ups and goals

Completion produces a `background_completed` socket event and a TUI notice.
`status.tasks` contains current summaries. The event/wake carries exit information,
stop evidence and a bounded output preview. Full captured output remains available
through `status`; normal core tool-output shaping/spill rules still apply.

With `wake: true` (the default), completion is queued as explicitly untrusted task
data. If the session is already working, it arrives through the existing steering
queue at a core boundary. If idle without a goal, the original worker starts one
wake-up prompt in the same conversation. No RPC child, second writer lease, new
permission mode or model switch is involved. `wake: false` still publishes the UI
and socket event but does not start or steer inference automatically.

A goal that must wait should declare:

```json
{"state":"blocked","reason":"waiting_background_tasks"}
```

That reserved wait can resume automatically when a result arrives, within the
SAME goal ledger and remaining budget. Paused, canceled, exhausted, unrelated
blocked goals and another goal's task results never fund new inference. Explicit
goal replacement clears queued wakes from the previous goal. A DONE declaration
is held as waiting while that goal has running or unread results, including the
race where a process exits just before the declaration. Receiving the result in
the model context, or querying its completed status, clears that pending result.
DONE evidence is still agent-reported; this is not independent proof that every
project requirement is correct.

Goal pause/cancel/time expiry cancels owned commands even when the model itself
is idle waiting for them. TUI exit, session replacement, reload and permission-mode
changes stop and join the old command owners. New work does not inherit a running
process with old grants. Session replacement discards old wake delivery rather
than replaying it on the replacement session. An ordinary TUI command after a
completed/canceled goal explicitly returns to normal session work; an exhausted
ledger still requires an explicit new goal or session change.

## Bounds and ownership

- At most four starting/running/canceling tasks and sixteen retained results per
  session. Read and forget settled results to make room; IDs are not recycled.
- A task timeout defaults to 120 seconds and is limited to 1..86400, so a long CI watch can request an hour or more. Goal deadlines
  also cancel commands. Boot-clock time includes suspend; process cleanup has its
  own bounded signal/drain grace and remains visible until reap.
- Combined stdout/stderr capture is 1..64 KiB (minimum request 1024 bytes). A flood
  stops at the limit; it does not accumulate an unbounded reader queue. Invalid
  UTF-8 is represented lossily, so its displayed string can expand. Wake previews
  are at most 8 KiB; socket history keeps its existing count/byte bounds and gaps.
- Each task has one owned supervisor thread, nonblocking pipes, its own process
  group and the sandbox's PID namespace. Cancellation escalates to kill; cleanup
  observes exit without reaping first so the process identity remains pinned until
  group cleanup is complete. Parent/thread death kills the sandbox monitor.
- No daemon, installed service, detached reader or automatic command resend.
  Task records/output are memory-only here. The normal TUI transcript can retain
  tool arguments/results and wake messages under its existing persistence policy.

The default-off `Sandbox::prepare_background` hook is necessary because the normal
synchronous runner collects output until return. It applies the same current
refusal checks and git-file preparation before returning the sandbox command;
`NetSandbox` clamps it through the existing offline policy. Unsupported sandboxes
fail closed. Existing synchronous execution is unchanged. The process supervisor,
tool, task state, wake delivery and accounting integration live in the new crate;
only this small guarded hook and CLI dependency construction touch existing code.

## Model-free verification

The actual TUI fixtures run sandboxed commands alongside fake model requests and
verify idle and in-flight wakes, retained session identity/context, useful work
before command completion, held premature DONE, same-budget goal wake-up, denied
execution, no-tools, rejected network arguments, read-only plan execution,
pause/cancel/deadline cleanup, TUI exit and session swap without late replay.
Owned-process tests cover output flood, timeout, cancellation, descendant cleanup,
concurrency/retention limits, full result reads/forget, foreign-session rejection,
offline preparation and protected git-alias refusal. No installed model, cloud
provider, production service or other machine is used. Worker-lane role/model/
budget integration and full-evening cloud acceptance remain outstanding.
