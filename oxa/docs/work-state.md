# Runner checkpoint integration

The runner is the sole writer of task state. `run_task_with_observer` accepts a
`CheckpointObserver: Sync` with `checkpoint(&Record) -> Result<()>`. Observers
consume public typed records; they never read or rewrite private `State` files.
The CLI supplies `FactObserver`, which maps each checkpoint to one typed fact in
the separately owned facts store. It never forwards prompts or diagnostic prose.

Checkpoints live outside writable Git binds in the protected per-repository
runner root. `<id>.json` stores version 2 private state with its task spec,
frozen execution inputs, offline policy, deadline and public record. `<id>.lock`
is a stable inherited kernel lock. Status is read-only and does not acquire it.
Facts storage receives a separate `facts/` child of this protected root, avoiding
collisions with task files. Standing data also remains outside Git.

`Record.checkpoint_revision` is repository-wide and strictly increases. The event
producer domain is the fixed string `runner`, not one source per task. Event
sequence numbers belong to the facts store and are distinct from checkpoint
revisions. Reservations that did not reach the task rename can leave revision
gaps; they must not create event-sequence gaps.

A short repository kernel lock serializes revision allocation, task-file
replacement, observation and acknowledgement. `.checkpoint-counter` contains
the last reserved revision and at most one pending task ID. A new writer must
recover that pending durable checkpoint before reserving another revision:

1. Reserve the revision and pending identity atomically, then sync the directory.
2. Write and sync the complete task checkpoint, replace its canonical name
   atomically, and sync the directory. The first task publication is create-only.
3. Call the observer with the durable public record.
4. Atomically acknowledge that observation in the repository counter.

A failure stops the batch and poisons that writer. Recovery replays the same
durable revision, including a lost acknowledgement after an append. The observer
must deduplicate that identity and reject different contents under it. Older
acknowledged task records are not replayed: the source table needs one bounded
runner high-water mark, without completed-task tombstones. A missing/corrupt
initialized counter fails closed rather than resetting revisions.
`recover_checkpoints(root, observer)` exposes that pending-observation recovery
before a lead reads its facts view; it never acquires a task lock, changes a task
file or starts a worker. The plain `status` command remains entirely read-only.

`checkpoint_kind` distinguishes `task_transition` from `worker_finished`. The
worker result is saved at acceptance/recheck with the latter kind; a separate
revision enters that phase before running checks. A crash between the two does
not restart the worker. A preparing transition can establish a `TaskStarted`
location; subsequent transitions update the closed step and optional session.
Worker completion, local acceptance, publication, external acceptance and merge
remain separate observations. Only a merge is landed work.

`blocked` and `unblocked` kinds each get their own durable revision. A failed
attempt records its block before the separate finished transition; a new attempt
explicitly clears that task block before its preparing location. `failure_step`
preserves a closed stage for terminal classification, including a save failure.
Changing a PR head does not clear a block or standing hold.

New planning refusals also get a durable block and separate finished transition,
without creating a worktree or starting a worker. Once the same task's plan is
fixed, normal admission clears that block. Refusing a live task, a different task
specification, any previously executed attempt or an unsupported checkpoint leaves its existing state untouched;
that rejected input cannot replace the facts of work already in progress.

`oxa work situation [--json]` and `oxa work events --after N [--json]` consume the
public bounded store APIs. Reads recover the one pending checkpoint observation
before returning a snapshot/page. JSON uses the common versioned documents; event
pages contain at most 64 entries and expose rotation gaps. Missing/corrupt stores
fail; reads never initialize an empty situation. Task admission initializes facts
only before the repository's first checkpoint. An initialized runner with missing
facts refuses admission rather than recreating history.

The existing stdout reader also reads the native status API at its saved
15-minute progress deadline. The child receives `--stall-timeout 900` and its
header checkpoints both the session ID and PID. Identity/phase/deadline validation
uses `incidents::detect_stall`; missing or stale data stays unavailable. A valid
stall gets a separate durable `worker_stalled` revision and incident. Repeated
reads of that same session/PID/progress period do not append it again. The task
keeps running, with the same wall deadline; no watcher thread or process is added.
Status and incident I/O stay off the parent's deadline loop. An anonymous socket
ends the reader when the parent has reaped the child, including after early EOF.

Records expose fixed outcomes and typed deadline interruption. Their `detail`
is bounded diagnostic context for the owner, not a facts payload: event adapters
select typed fields and fixed reason codes and never emit prompts or raw logs.
Saved head/tree IDs describe the actual saved commit. They do not prove that an
ignored helper or build artifact was absent while local checks ran; publication
must verify commit-bound acceptance separately.
