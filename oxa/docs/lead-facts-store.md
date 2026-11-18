# Runner facts storage

`oxa_work::events::Facts` is the L1 storage module for #629. The runner remains the sole task
state writer. These modules never open its task files, invoke a model or start a background
process. The lead consumes the public read APIs rather than logs or private runner records.

The runner supplies exactly `<protected repository root>/facts`, outside writable workspace
and Git binds. `Facts::initialize(root)` explicitly creates a fresh store; `Facts::open(root)`
opens/replays an existing one. Missing, corrupt, unsafe or partial initialization is an error,
never an empty situation. Directories are private (0700), files private (0600), and files and
path components are checked without following symlinks. It opens only its fixed storage leaves.

## Append after a durable checkpoint

Call `append(Checkpoint { source, revision }, at_ms, fact)` only after the corresponding
observation is durable. Stop before another action if append fails. The runner's source is
`runner`; its checkpoint revision is monotonic per repository. A short repository checkpoint
lock serializes allocation, durable task rename, callback and acknowledgement. The runner
recovers a pending task/revision before allocating again and does not re-emit older acknowledged
task records. Reserved checkpoint revisions may have gaps; event sequence numbers do not.

Other producers use their own stable emitter domain and monotonic revisions, not a fresh source
per task. The bounded source table holds the latest observation for up to 16 emitter domains;
it retains no unbounded per-task tombstones. Retrying the latest source/revision with the same
fact returns its original event and timestamp, even after journal rotation. Different facts
under that identity and stale revisions are errors. Sequence exhaustion is an error.

Standing initialization reserves one of those 16 slots before its first mutation;
ordinary producers cannot consume that reservation. The `standing` checkpoint
source, `StandingChanged` observations and `standing:` ask-ID namespace belong
to the standing store's protected prepared-append path, not public `Facts::append`.

Each append validates the projected situation before reserving the event. It then saves a
pending observation in `facts.json`, appends and syncs the journal line, commits the source
cursor and atomically replaces `situation.json`. Atomic replacements sync the file and directory.
Recovery replays that reserved observation across a torn final line, interrupted rotation,
lost acknowledgement or lagging situation. Complete malformed lines, unreserved torn tails,
conflicting contents and unrecoverable gaps fail closed.

`TaskStarted` supplies the initial preparing location (task, host label, branch and worktree).
Later `TaskTransition` observations update its step/session; `finished` removes that running
attempt. A transition without a known running location is an error. Worker completion, local
acceptance, publication, external acceptance and merge stay distinct. Only `PrMerged` enters
the landed list. Blocks and asks persist until explicit `Unblocked` and `AskClosed` facts;
updating a PR head cannot silently clear either.

## Bounded reads

`situation()` returns a validated `oxa_proto::work::Snapshot` with its exact `as_of_seq`.
It is capped at 64 KiB and 128 total rows. Running work, local acceptance awaiting publication,
published work, blocks and asks are never silently dropped to fit. Capacity failure stops the
next action. Landed/incident history retains at most 16 rows each and may shrink to fit;
`history_truncated` records that loss. An accepted/published pair is retired only by the matching
head's merge observation. Standing revision/goal remain unknown until the standing integration.

`events_after(after, limit)` takes an exclusive repository cursor and a limit from 1 to 64.
The two journals (`events.jsonl` and `events.previous.jsonl`) rotate at 256 KiB each. Pages
report `first_available`, `next` and an explicit `gap`; a consumer seeing a gap reloads the
situation rather than inventing old transitions. A cursor ahead of the journal is an error.

`incidents::recent(&facts)` returns the bounded incident projection from that same situation;
the journal is its durable source, not a competing incident/task-state writer. Reads recover
only derived facts metadata/projections under the facts lock and never alter task state.

## Stall incidents

`incidents::detect_stall(status_bytes, expected_session_id, expected_pid, now_ms)` consumes a
bounded status snapshot supplied by the runner. It validates identity, phase, progress, timeout
and deadline; stale invocation identity or malformed data is an error. It returns a typed
`Stall` only when the conservative deadline has elapsed. Sessions without an active deadline and finished sessions do not
stall. The caller may append `Stall::incident(task)` through its durable event boundary.

Stalls are incidents only: this module never wraps, cancels, kills or polls a worker. The
runner passes `--stall-timeout 900` for the decided 15-minute threshold and separately owns
the per-task wall-time limit and escalation. No second watchdog is introduced.

The runner owns `oxa work situation`, `oxa work events --after N`, checkpoint adapters and
action wiring. The lead owns the status/ask cards. The common schema is described in
[work-facts.md](work-facts.md); storage/recovery tests are in
[events/tests.rs](../oxa-work/src/events/tests.rs).
