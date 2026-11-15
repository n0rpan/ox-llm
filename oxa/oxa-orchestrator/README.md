# Opt-in orchestrator foundations

Opt-in foundations extracted from rollup #88 for [issue #282](https://github.com/oxstack/ox-llm/issues/282).
Private audit storage, JSONL pipe transport and an explicit tool-free native
worker owner are present here. No worker, listener or CLI hook starts on import;
no replay or installed service is enabled. Journal inspection is explicit.

The `core` feature exports `AuditJournal`. Its caller explicitly supplies a new
journal path under an existing mode-0700 operator-owned directory and a byte
limit in 1..=16 MiB. Ancestors must be operator/root controlled; symlinked
ancestors and parent traversal are refused. Same-UID processes and root remain
the trusted namespace boundary. Records are mode 0600, create-only and fsynced. Creation syncs the pinned parent
directory before returning a usable writer, so successful records also have a
persisted directory entry. Filesystem sync failures are propagated.

Each accepted JSONL record has schema, sequence, elapsedMs, worker, event and
explicit caller-supplied details. `run_started` records name the ownership
protocol. The caller is trusted and must supply bounded labels/hashes/counts,
not raw prompts, outputs or credentials. The library does not capture any of
those values implicitly, and this is not a replay format or an authorization to
signal a recorded PID.

A nonblocking inode lease rejects another controller. An append failure closes
further admission and retains ownership until the writer is dropped. Validation
and size rejection happen before writing; an actual storage error can leave a
partial trailing record, which is uncommitted and must never be replayed. The
writer does not promise rollback after a failed write or sync. Normal
release explicitly unlocks even if an unrelated pre-exec fork temporarily holds
the open-file description; CLOEXEC prevents inheritance across exec. Fatal death
still follows the kernel's last-description-close rule. The writer is not cloneable.

```sh
cargo check -p oxa-orchestrator --no-default-features --lib
cargo test -p oxa-orchestrator
cargo test -p oxa-orchestrator --features core
cargo clippy -p oxa-orchestrator --all-targets -- -D warnings
```

Normal builds leave `core` off. The test-only self dependency enables it for the
ordinary workspace test command too, so CI's default test inventory includes the
seven actual filesystem/fork regressions. Existing binaries do not depend on this
crate. No workflow or scanner change is needed. See [slice plan](SLICES.md) for
the remaining dependency order and preserved rollup sources.

## Bounded JSONL transport

With `core`, `pipes::JsonPipes` takes the parent's explicitly supplied child
stdin/stdout/stderr handles and makes them nonblocking. It creates no process,
thread or listener. The caller remains responsible for sandboxing, permission
checks, cancellation, deadlines and confirmed descendant cleanup/reaping. EOF
reports only pipe state; it does not establish process completion. Dropping the
transport closes its handles and never joins a reader waiting for EOF.

Each poll services at most eight writes and eight 4-KiB reads per output stream.
Pending input and one output JSONL record are each capped at 256 KiB. Input
serialization is bounded before enqueue. Both request and response records must
be JSON objects. Malformed/non-object/overlong records and an unterminated final
record fail the transport; blank lines are ignored. Diagnostic bytes are discarded
and count toward the shared lifetime output allowance. Reads use at most the
remaining allowance plus one detection byte, and `output_bytes` includes that
observed overflow byte (saturating only at the counter's integer limit).

An I/O, protocol or admission error permanently closes input and discards pending
requests. `close_input` can also explicitly discard input while allowing output
to drain. Polling can successfully return no records without EOF. This slice
adds no native RPC launch profile, provider model selection or automatic retry.
Tests use owned local subprocesses with cleared environments and kill/wait guards;
there are no provider calls. The allowance-overflow regression fails before the
accounting fix and passes afterward.

## Explicit tool-free native process owner

With `core`, `native::NativeWorker` launches only an operator-selected executable
through an operator-selected bwrap executable. `VerifiedLocal` records the
operator assertion that its literal loopback route cannot forward to paid/cloud
inference. Address syntax and model validation cannot prove that assertion. This
slice neither authorizes cloud inference nor reads provider credentials.

The fresh filesystem exposes read-only runtime libraries and the chosen program,
private HOME/tmp/work and a new PID/user namespace. Environment/configuration,
source checkout, MCP/extensions/skills and tools are not inherited. The native
profile uses current main's RPC/plan/offline flags and removes the obsolete
OXA_PROFILE setting. Approved workspace mounts belong to a later integration.
The supplied role must already be approved for delegation. It is passed directly
to the trusted executable and is not journal metadata.

The caller must drive `poll`/`queue` to enforce the monotonic wall deadline; there
is no background watchdog. Transport/admission/deadline failure forces cleanup.
`stop` permits at most one second of graceful abort, then bounded SIGKILL/reap
observation. Confirmed child exit is retained explicitly; a failed cleanup is an
error, never a process-stopped claim. Pipe EOF is not process completion. Group
signals use only the retained unreaped Child; recorded PIDs are never adopted or
signalled. PID-namespace teardown covers descendants that change process group.
Drop attempts the same cleanup, but callers requiring evidence must call `stop`
and check its result before dropping the owner.

The real-bwrap fixtures use a trusted mock runtime, not the production oxa binary
or a paid model. They verify flags, cleared/private environment, hidden source,
read-only runtime mounts, a distinct PID namespace, RPC pipe roundtrip, grace,
deadline/protocol/admission cleanup, closed runtime stdout with a live wrapper,
and an inherited
socket lease closing after a setsid descendant is terminated. Process ownership
is not a scheduler, token-accounting policy or proof of provider billing stop.

## RPC policy, settlement and observed usage

With `core`, `rpc::RpcSession` is a pure state machine for the tool-free profile.
It performs no I/O. The owner sends its initial get_state, admits returned
commands through the bounded pipe queue and drives process/deadline ownership.
Policy verification checks model/plan/idle state, disables retries/compaction,
then reads state again before releasing the prompt. Command identities and
pending steering acknowledgements are bounded. Admission is not model delivery.

Assistant updates and final messages contribute observed input/output/cache
usage across the worker lifetime. Progress is replaced by final usage rather
than counted twice. Missing, decreasing or overflowing usage cannot establish
success. Values are last valid observations, not a complete ledger settlement
or hard pre-request token/monetary ceiling. A threshold overrun is retained and
attributed as TokenBudget, never silently clamped to the configured allowance.

A completed reply requires prompt acknowledgement, settlement, valid final usage
and a normal stop. A length stop is Truncated with retained partial text and
observed usage; an unfinished assistant cannot reuse a previous message's stop.
Validation is staged, preserving the prior valid partial on rejection. Tool/UI
requests and policy changes fail closed. Records/events and serialized outgoing
prompt/steer commands are bounded; escaped JSON cannot bypass input admission.
No transcript is automatically logged. The owner must stop/reap the process on
an error or terminal outcome before publishing a finished worker.

This makes truncation explicit, but does not fix the live smoke's configured
512-token response ceiling. The owning worker/request-budget integration must
still expose and enforce a per-response allowance. No external driver ceiling
is raised here. The integrated test uses an owned mock runtime in real bwrap;
it proves policy/accounting/cleanup wiring, not production inference or billing.

## Explicit tool-free fan-out/fan-in

With `core`, `supervisor::run` validates one complete plan before journal creation
or worker launch. It accepts up to 128 workers and 32 concurrent lanes, bounds
wall times to one day and reserves every worker's lifetime output/token allowance
against run totals. Task/role contents must already be approved for delegation;
all routes carry the operator's VerifiedLocal assertion. There are no tools,
workspace mounts, automatic retries, remote routes or CLI hooks in this slice.

`controls()` creates a 32-entry nonblocking queue. Individual cancellation covers
queued/live workers; sticky cancel_all remains available when the queue is full.
Steer responses acknowledge RPC queue admission, not model consumption. Controls
and each pipe poll get bounded work per loop. Trusted observer callbacks must
return promptly. Ready means policy verified; working means prompt admitted to
the pipe queue. Cancellation/deadline is checked again at that admission boundary.
No model work is admitted after an observed global terminal condition.

Every required lane produces an attributed report, sorted in plan order, with a
result, last valid observed usage and explicit NotStarted/Stopped/Unconfirmed
cleanup. Success requires every lane to be Completed with confirmed process stop.
Truncation, partial failure, timeout, cancellation and budget exhaustion remain
separate outcomes. Unconfirmed cleanup cannot publish a completed worker or
run_finished. Final pipe data is drained within its bounds after child exit,
rather than treating reaping alone as loss of the remaining final response.

The journal holds its exclusive lease throughout ownership and records metadata,
content hashes and process history, never raw task/role/result text. run_finished
retains workers/completed counts. The tool-free profile's new truncation and
unconfirmed-cleanup cases have explicit native inspection support; no automatic
reconciliation or recorded-PID signalling is introduced here. An infrastructure
or journal error fails the call and triggers owned-process Drop cleanup; it is
not a successful report or proof of billing termination.

Reported usage can overrun an observed token threshold before a response settles.
The loop also checks aggregate observations, but these are not hard pre-request
or monetary caps. Output counts include the single overflow detection byte.
The configurable per-response token allowance still belongs to its request-budget
integration slice. There is no paid run in this implementation or its tests.

Real-bwrap mock tests cover parallel/serial lanes, complete and truncated fan-in,
a large final reply followed by process exit, full control-queue cancellation,
steer admission, targeted queued/live cancellation, per-worker/run deadlines,
output/token overruns and pre-launch validation. Removing the final cancellation
guard makes the mock-side-effect regression fail; restoring it passes. This is
local protocol/process acceptance, not a production-model evening-run claim.

## Native journal inspection and exclusive claim

With `core`, `recovery::InspectedRun::inspect` reads an explicitly selected
private/operator-owned single-link plain journal. It walks protected parents
without symlinks, reopens the validated inode and checks metadata before/after
capture. Bounds are 16 MiB total, 64 KiB per complete/incomplete line and 100,000
records. Complete records require schema/sequence/monotonic-time consistency,
a complete worker manifest before lifecycle transitions, valid identities/hashes
and consistent completion counts. Unknown profiles/events and A2A/resume records
are refused in this native-only slice.

An incomplete final line is fingerprinted with the file but never promoted to
an accepted record, even if it looks like complete JSON. The metadata-only
snapshot exports no raw task/role/result or tail content. Recorded process IDs
are historical data, never live signalling authority. Recorded completion is
not proof that a provider or background process is currently stopped.

`ExclusiveRun::claim` additionally takes the cooperative inode lease and rejects
active or poisoned writers that still hold it. Legacy files without the declared
ownership protocol can be inspected but cannot be claimed. The guard must stay
alive for any future recovery operation; it establishes exclusive journal access,
not worker cleanup. The inspection API has no network call, replay, resume, submission,
file mutation/deletion or recorded-PID signal.

The current tool-free producer uses profile v2 and records explicit cleanup.
Post-spawn pipe-setup failures preserve their distinct spawn/reap evidence instead
of claiming the worker never started. v1/legacy records remain readable; where
they lack cleanup evidence the snapshot retains Unknown (None), including the
ambiguous v1 starting-without-spawn case. Claiming a file never upgrades that
unknown status. Native truncation and unconfirmed cleanup are retained explicitly.

Tests read actual supervisor-produced journals, enforce live/failed-writer lease
exclusion, inspect valid/truncated prefixes, reject lifecycle/format mutations
and links/FIFOs/oversized files, and exercise the 100,000-record boundary. Startup
failure tests reap a real owned child. Explicit original-agent reconciliation,
checkpoint consent, resume and their CLI surfaces remain subsequent slices.
