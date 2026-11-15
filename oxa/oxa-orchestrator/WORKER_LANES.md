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
loopback proxy cannot forward to paid inference. Version 1 does not enable cloud
workers or validate cloud cost ceilings. See [GATEWAY_WORKERS.md](GATEWAY_WORKERS.md)
for explicitly authorized version 2 gateway profiles. Only mocks have been run.

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

The [mixed-provider review demo](REVIEW_LANE_DEMO.md) additionally drives the
actual TUI and gateway with native Responses/Messages mocks: an editing GPT author
returns its collected patch, the parent passes the exact bundle to a tool-free
Sonnet reviewer, and both verified results return to that same parent. The source
stays unchanged and all three usage ledgers reconcile with gateway rows.

The only additional existing-crate hooks in this milestone are feature-gated
footer population and copying of the reserved `oxa.worker.` status rows in
`oxa-interactive/src/app.rs`. Worker registration uses the existing optional
session augmentation hook. All worker implementation lives in this new crate.

## Per-response output allowance

An operator profile may set `"response_tokens": 2048`. Omission preserves the
model/request output limit; zero and invalid integer values are rejected. The
library equivalent is `LocalInference::with_response_tokens`. This is separate
from the lifetime `budget.tokens`, wall/turn deadlines and retained `output_bytes`.
Model tool arguments cannot set or override it, and steering/retargeting does
not replace the captured profile policy.

The owned child receives this limit through its cleared private environment.
Its in-process engine adapter clamps both published model output allowance and
every inference request to the smaller existing allowance, including summaries;
zero-output prefills remain zero. The parent requires the exact policy
acknowledgement before dispatch. Missing/mismatched acknowledgement is a policy
violation and triggers owned process cleanup. Shared worker status exposes
`response_tokens`; the verified-policy journal records `responseTokens` without
prompt or response contents. No external relay is responsible for enforcing it.

A settled length-limited native reply is `truncated`, not DONE, and a goal stops
blocked with reason `native_response_truncated`. Its settled usage remains in
the cumulative goal ledger. Canceled, failed or unsettled inference does not
become known usage. A truncated result cannot be published as an approved
completed proposal, and no automatic retry or limit increase follows truncation.
The allowance caps requested output; it does not guarantee provider billing or
prevent lifetime observed-token threshold overshoot within an in-flight request.

Local regressions demonstrate clamping for chat, compaction and prefill, unchanged
smaller requests, profile/grant rejection, exact child acknowledgement and real
sandboxed RPC workers. The same response truncates at 512 and completes at 2048;
both retain observed usage and confirm owned process cleanup. The real goal
regression fails before the usage fix: settled truncation was incorrectly
reported as unknown. The real CLI/tmux demo also checks the 2048 limit in status
and the owned fake gateway's actual request records. No paid provider is used.

Worker token allowances follow the observed-threshold contract in
[session goals](SESSION_GOALS.md). The native child receives its immutable
lifetime threshold from the owner and gates every SDK call locally, without
waiting for the supervisor to poll. A settled overrun is retained in the outcome
and exposed as `token_overrun`; further chat/summary/prefill admissions stop.
The per-response output allowance remains separate. Strict pre-request ceilings
are deferred to [issue 445](https://github.com/oxstack/ox-llm/issues/445), and the
gateway's daily budget remains the hard monetary limit.
