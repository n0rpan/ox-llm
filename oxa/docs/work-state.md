# Runner checkpoint integration

The runner is the sole writer of task state. `run_task_with_observer` accepts a
`CheckpointObserver: Sync` with `checkpoint(&Record) -> Result<()>`. Observers
consume public typed records; they never read or rewrite private `State` files.
The CLI currently supplies a no-op while the separately owned facts store is
integrated. The hook is not itself an event journal or situation implementation.

Checkpoints live outside writable Git binds in the protected per-repository
runner root. `<id>.json` stores a versioned private state with its task spec,
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

Records expose fixed outcomes and typed deadline interruption. Their `detail`
is bounded diagnostic context for the owner, not a facts payload: event adapters
select typed fields and fixed reason codes and never emit prompts or raw logs.
Saved head/tree IDs describe the actual saved commit. They do not prove that an
ignored helper or build artifact was absent while local checks ran; publication
must verify commit-bound acceptance separately.
