# Session worker lanes (issue #94, in progress)

Build the CLI with `--features orchestrator`. The existing shared-session TUI can
then expose `worker_lane`, a tool for starting and steering native oxa goals over
authenticated literal-loopback A2A. The main session keeps its own conversation;
each worker keeps one native process and conversation across goal turns.

Set `OXA_SESSION_CONTROL_DIRECTORY` as described in [SHARED_SESSION.md](SHARED_SESSION.md)
and `OXA_WORKER_PROFILES` to an absolute, private JSON file under the canonical
operator agent directory (normally `$HOME/.oxa/agent`). The file must be owned by
the current user, mode 0600, single-linked, and at most 64 KiB. Symlinks and paths
inside the workspace are rejected. Invalid configuration installs no worker tool.
`OXA_BACKGROUND_TASKS` is independent and need not be enabled.

Example operator configuration, using an operator-verified **local-only** engine:

```json
{
  "version": 1,
  "verified_local_only": true,
  "parallel": 2,
  "total_tokens": 200000,
  "total_output_bytes": 2097152,
  "profiles": [
    {
      "name": "author",
      "role": "Implement the assigned change and report concrete checks.",
      "engine": "127.0.0.1:8080",
      "model": "operator-approved-local-model",
      "budget": {"wall_ms": 60000, "tokens": 100000, "turns": 8},
      "turn_ms": 10000,
      "output_bytes": 1048576,
      "files": ["src/example.rs"],
      "edit": true,
      "allow_new": []
    }
  ]
}
```

`verified_local_only` is an operator assertion about the route, not proof that a
loopback proxy cannot forward to paid inference. This stage does not enable cloud
workers or validate cloud cost ceilings. No cloud experiment has been run.

Profiles fix role, model, route, goal budget, per-turn deadline, output cap, and
file grants. The model supplies only a profile name and goal. Workspace files
pass through the main session's normal read policy before a private snapshot is
captured. Editing additionally requires Workspace mode. Worker proposals never
apply to the source automatically. Workers have no shell, ambient host context,
external extensions, credentials, or tools beyond their approved file grant.

## Tool operations

```json
{"operation":"start","profile":"author","goal":"Implement and verify the assigned change"}
{"operation":"list"}
{"operation":"status","worker_id":"<returned ID>"}
{"operation":"steer","worker_id":"<returned ID>","revision":1,"text":"Preserve the existing public API"}
{"operation":"pause","worker_id":"<returned ID>","revision":2}
{"operation":"retarget","worker_id":"<returned ID>","revision":3,"goal":"Address the review comments"}
{"operation":"resume","worker_id":"<returned ID>","revision":4}
{"operation":"cancel","worker_id":"<returned ID>"}
{"operation":"forget","worker_id":"<returned ID>"}
```

Read the current child goal revision before controlling it. A control response
means queued; `last_control` in status reports subsequent admission or rejection.
Steering is available while inference runs. Retargeting settled paused/blocked
work preserves the entire existing budget and ledger. Unknown usage stays
unknown, and token-bounded work cannot resume when its remaining allowance cannot
be established. A time-only goal may set `budget.tokens` to null and separately
set `reported_tokens` for its native observed cap and lifetime reservation.

At most four workers run concurrently and sixteen records are retained. Every
admitted worker reserves its complete token/output allowance from a separate,
operator-funded lifetime pool. Cancellation, failure, forgetting, goal changes,
and session rebinding never refund that reservation. The main session token
ledger does not include worker usage; inspect each child's ledger separately.
Observed token limits are not prepaid monetary ceilings.

## Results, lifecycle and TUI

Socket `status.workers` lists workers, fixed profiles/models, child goal ledgers,
and the last correlated control event. `worker_status` events report blocked
workers; `worker_completed` follows a verified terminal A2A result and native
cleanup report. Result hashes are matched against the locally owned report.
Transport failures remain visible failures rather than successful completion.
There is no automatic replay of an uncertain initial dispatch.

Worker results wake the SAME main core session, including while its inference is
running. Text from workers is untrusted data. A main goal declaring DONE while
workers or unread results remain is held blocked with `waiting_workers`.
Completion resumes that reserved waiting state if the main budget permits;
operator pauses, cancellation and budget exhaustion never resume automatically.
Use `worker_lane status` for the bounded full report and private review proposal.
Read a stopped result before forgetting it. The footer displays up to four live
workers and four retained terminal workers with profile, model, state and budget
usage. Other extension status rows are preserved.

Parent pause/cancel, goal deadline, mode changes, session replacement and TUI exit
cancel owned workers. Shutdown joins their supervisors and native owners. No
worker authority survives into a different session. Configuration is captured
when dependencies are built; changing the file does not widen live grants.

## Reproducible mock demonstration

```sh
cargo test -p oxa-orchestrator --features interactive-control --test shared_tui -- --workers-only
```

The harness doubles as the actual native RPC worker executable. It uses owned
loopback fake inference servers and bwrap, without installed models or provider
keys. The cases exercise two fixed roles/models over persistent goal turns,
model-issued steering during inference, shared-session result wakes, prevention
of premature DONE, parent cancellation, Esc settlement with unknown usage, and visible TUI worker status. Broader
cloud reliability and the full working-evening acceptance run remain outstanding.

The blocked-worker case waits for the worker's event, then uses model-issued
retarget and resume controls. It checks unchanged worker identity, conversation
and budget, exact cumulative tokens across both requests, elapsed time including
the wait, the clarification in the next inference, and reaping before completion.

The only additional existing-crate hooks in this milestone are feature-gated
footer population and copying of the reserved `oxa.worker.` status rows in
`oxa-interactive/src/app.rs`. Worker registration uses the existing optional
session augmentation hook. All worker implementation lives in this new crate.
