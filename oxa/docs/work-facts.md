# Lead facts contract

`oxa_proto::work` is a runner-independent vocabulary for #629. Lead UI fixtures can use these
types before the runner's event store is available. The module has no filesystem access, worker
launcher, task-state writer, authorization policy or background thread.

`Event { seq, at_ms, checkpoint, fact }` describes one durable observation. Sequence numbers start
at one and never reset on rotation. `Checkpoint { source, revision }` is the producer's stable
replay identity: the future event store must deduplicate a replay of that identity and reject
different contents under it. Sources have independent revision spaces; the runner supplies its
checkpoint revision after the durable checkpoint rename. Sequence exhaustion is an error.

Facts distinguish `WorkerFinished`, `LocalAcceptance`, `Published`, `ExternalAcceptance`,
`Enqueued` and `PrMerged`. A worker's `done` result says nothing about publication or landing.
Only `PrMerged` describes landed work. Acceptance carries an evidence reference; publication and
external acceptance bind a full PR head. A head-independent `Subject::PullRequest(number)` is
available for blocks, asks and future standing holds. Task, host, repository and inbox subjects
also exist, including tasks that do not yet have a PR.

`TaskTransition { task, step, session_id, reason }` reports every durable runner step:
`preparing → worker → acceptance → repair/recheck → saving → finished`. Each observation gets
its own checkpoint revision; worker completion and local acceptance may have separate revisions.
`TaskStep` is closed, `session_id` is optional until known, and `StepReason` is a closed code
such as `checks_failed`, `session_unavailable` or `deadline`, not diagnostic prose. A finished
step requires a reason and says only that this local attempt ended. It does not imply acceptance,
publication or merge. These observations are routine; a separate block/ask/incident carries any
attention condition. Running situation rows expose their current step and cannot be `finished`.

`Snapshot` is a materialized read view: `as_of_seq`, observation time, standing revision, optional
goal, running tasks (host, branch, worktree and optional session), locally accepted work,
publications, landed PRs, blocks, asks and incidents. It is not authoritative task state.
`history_truncated` explicitly marks a shortened terminal/incident history. The writer must never
discard running tasks, blocks or unanswered asks to fit the cap; a capacity error must stop the
next action. Reading a missing or corrupt store must return an error, not an empty snapshot.

`EventPage { after, events, next, first_available, gap }` is an exclusive-cursor page. Events are
contiguous from `max(after + 1, first_available)`. `next` is the last returned sequence, or
`max(after, first_available - 1)` for an empty page. `gap` is true when retention has removed
events after the requested cursor; a consumer must reload the situation instead of inventing
those transitions. A cursor at the maximum sequence can only return an empty page.

`Fact::class()` applies the fixed mechanical/routine/attention table. Producers do not supply a
wake class. Transient retries, no-build requeues, refill and verified clean branch updates are
mechanical. Lifecycle/publication/acceptance/merge observations are routine. Blocks, asks and
attention incidents (including stalled workers, rejected heads, budget limits and unread inbox
messages) need attention. Wake delivery and coalescing belong to the runner/lead integration.

`Event`, `Snapshot` and `EventPage` expose `to_json()` and `from_json(bytes)`. Documents carry a
distinct `type` and version 1; missing/unknown fields, unknown kinds, inconsistent cursors,
abbreviated commit IDs and invalid evidence kinds are errors. JSON input is capped before parsing
(8 KiB/event, 64 KiB/situation, 256 KiB/page), collections at 128 items, and strings are bounded
single lines. These limits also apply when encoding Rust fixtures. Error messages contain only
fixed reasons. Producers supply identifiers and checked evidence references, never prompts,
tool arguments, raw logs or diagnostic output. Bounds do not scrub secrets from arbitrary text.

The executable contract fixture is in [work_facts.rs](../oxa-proto/tests/work_facts.rs). The event,
situation, incidents and standing stores will live in the runner crate. The runner remains the
sole writer of task state and owns CLI/action wiring; the lead consumes the public read APIs.
