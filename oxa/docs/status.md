# One status view, and a targeted work stop (design)

Two related surfaces, one read-only slice now and one specified but not implemented:

1. **The status view** is implemented in this change: `/status` in the terminal interface, the
   same command outside it, and bare `oxa status` all print the same rows.
2. **The targeted work stop** (`oxa work stop <id>`) is specified here and is not implemented.
   Nothing in the current binary signals another process.

Both keep the existing guarantees: no engine, provider or sign-in contact for a status read, no
unvalidated PID, no new task-state writer, and no change to sandbox, trust, permission or
cancellation behavior.

## The status view

### Where it appears

| Surface | Command | What it reads |
|---|---|---|
| terminal interface | `/status` | the view model and the session's engine facts |
| print, JSON and RPC `prompt` | `/status` | the running session's typed state |
| a shell | `oxa status` | the current directory's saved session and its worker snapshot |
| a shell | `oxa status <id-or-file>` | exactly that session's version-1 snapshot JSON (unchanged) |

### Rows and where each fact comes from

A row is printed only with a fact some typed API really returned. A row without a fact prints
`unavailable (<reason>)`; it never prints a zero, an empty string or an invented value.

| Section | Row | Terminal `/status` | Headless `/status` | bare `oxa status` |
|---|---|---|---|---|
| Session | Session file, Session id, Name, Messages | view model and session blocks | session state and stats | session store and saved entries |
| Session | Tokens | session totals | session totals | the worker snapshot's counters |
| Session | Context | context tokens over the window | context tokens over the window | unavailable: the snapshot records no context size |
| Session | Cache, last request | last request's cached/prompt | last request's cached/prompt | the snapshot's cached/prompt counters |
| Turn | Turn | live activity text, or `running` while busy | busy state, in-flight request, last job outcome | the snapshot's phase, last tool, stall signal and outcome |
| Turn | Model | the model and where it runs | the model and where it runs | unavailable: the snapshot records no model |
| Queues | Steering, Follow-up | the UI's queues | the session's queues | unavailable: the queues belong to a live process |
| Queues | Engine queue | engine-reported requests waiting | engine-reported requests waiting | unavailable: no engine is contacted |
| Engine | Engine endpoint, Engine model | the endpoint and model read at start | the endpoint and model read at start | unavailable: no engine is contacted |
| Work | Background jobs | unavailable: no registry reaches the UI thread | the session's job registry | unavailable: jobs live in the worker process |
| Work | Child sessions, Goal, Lead | unavailable | unavailable | unavailable |
| Budget | Cost | unavailable | unavailable | local spend, or that the provider charge is not reported |

The three always-unavailable Work rows are deliberate: there is no child-session listing, no
goal record and no lead facts for a session yet. A future goal or lead surface adds a row to
this view; it does not add a parallel status command.

The turn row's source is the surface's own state, so its wording differs by surface: the
terminal interface shows what is happening now, the headless command shows the running
session's job state, and `oxa status` shows the saved snapshot, including a failed outcome and
an expired stall deadline. A snapshot phase is not a run outcome: only the saved outcome names
a failure.

### Choosing the session for bare `oxa status`

Bare `oxa status` reports the most recently modified saved session of the current directory,
which is the session `--continue` would open. One rule, one implementation: `oxa_core::latest_of`
is what both use. A directory with no saved session prints `No saved session for this
directory.` and the rows with their reasons, and exits successfully; it is not an error to have
nothing saved yet.

An explicit session id or file keeps its existing behavior and output: the validated version-1
`oxa_status` JSON object. It still errors when the snapshot is missing, malformed, oversized,
mismatched or not a regular file.

### Read-only guarantees

* A status read never contacts the engine, a cloud provider or a sign-in endpoint. The CLI test
  runs with a dead loopback endpoint and a cleared environment.
* No process is started, no lock is taken, no task checkpoint is read or written, and no file is
  created or changed. Reading a snapshot never refreshes its progress or clears a stall.
* Rows are static text: no animation, colour or timing, so a terminal reader and a screen reader
  see the same content. The rows are plain ASCII labels.
* `--offline` changes nothing here because nothing in the path touches the network.

### Tests

* The view itself: row order, sections, the unavailable wording, and that an empty value never
  becomes a fact.
* Headless `/status`: the session rows, the job registry count and the unavailable Work and
  Budget rows.
* Terminal `/status` with a synthetic view model: idle and working turns, both queues, a local
  and a cloud model, and the unavailable rows.
* Bare `oxa status` with synthetic sessions and snapshots: the newest session of the directory
  is chosen, a failed/idle/stalled turn is reported, a missing snapshot and an empty directory
  are stated, the explicit form still prints JSON, and a dead endpoint plus `--offline` succeed.
* A pseudo-terminal run of the real binary types `/status` and asserts the static rows on
  screen.

## The targeted work stop (specified, not implemented)

`oxa work stop <id>` stops one run this machine owns. It is not a general kill: it is a
runner verb for the runner's own tasks, and it refuses everything it cannot positively identify.

### What identifies a target

A stop target is the task checkpoint `<id>.json` in this repository's protected runner storage
root, together with the stable `<id>.lock` kernel lock beside it. The record supplies the task
id and the checkpoint revision the owner saw; the lock supplies the identity of the process.

A PID is never sufficient and never the input. A validated target is a process that:

1. holds a write `flock` on that exact lock file, proven by the lock's device and inode in
   `/proc/locks`, or, for an inherited lock whose original owner died, by the lock description
   in the descriptors of a same-user `oxa` worker or supervisor process (the evidence the
   read-only `oxa work status` already gathers);
2. is the process the checkpoint's current step belongs to, with the record not at a finished
   step;
3. has an unchanged start time (`/proc/<pid>/stat`) and lock description at the moment of the
   signal, re-checked after the identity was resolved.

Anything else is refused, including a PID supplied by hand, a PID that now belongs to another
program, a lock held by a different user, a task from another repository's storage root, and a
task whose record says it is finished.

### Graceful cancellation

The stop reuses the runner's existing two-stage termination, the same one its own deadline
path uses: first `SIGTERM` to the validated worker, which makes a supervised child exit at once
and leaves the supervisor to record the interruption; then, after the existing kill grace
(ten seconds), `SIGKILL` only when the identity still matches. The supervisor's normal
settlement writes the interruption, so no second state schema, revision space or writer is
introduced.

A stop never deletes a session, a worktree, a checkpoint, a status snapshot or a branch. Saved
state is retained so the owner can inspect what the run had done and rerun the task file.

### Status receipt

Every stop invocation prints one bounded receipt:

* the task id and the checkpoint revision the owner saw;
* the identity it validated, as a form that cannot be replayed as a PID (`pid`, process start
  time and lock inode);
* the action: signalled, already finished, refused with a fixed reason code;
* the settlement it observed: interruption recorded, or the target still running after the
  grace.

Refusals carry a fixed reason code and a non-zero exit, never a partially applied stop. The
durable record of the stop remains the runner's checkpoint transition and its facts; the
receipt is output, not a new store. A repeated stop on a finished target is a refusal with
`finished`, so it cannot be mistaken for a second cancellation.

### Out of scope

Goal, lead, child-session and budget views; a stop for a process the runner did not start; any
change to task state durability, admission, budgets or engine behavior. Those arrive with their
own tasks, and this document is updated with them rather than replaced.
