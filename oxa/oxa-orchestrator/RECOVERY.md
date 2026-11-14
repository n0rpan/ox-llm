# Inspect interrupted runs and reconcile known tasks

The default-off orchestration feature adds a read-only command:

```sh
./target/debug/oxa orchestrate-inspect \
  --journal ./run-directory/run/transcript.jsonl
```

Native per-task RPC journals under `agents/` can also be inspected. Help is inert.
Inspection reads one explicitly selected journal and prints terminal-safe JSON.
It does not scan run directories, read agent configuration or credentials, write
state, start a worker/listener, contact an engine/agent, replay input or signal a
PID. A successful exit means the captured journal parsed, not that the run or
workers succeeded or have stopped.

The snapshot distinguishes queued workers, started workers without a durable
task binding, known A2A bindings and recorded terminal outcomes. It reports the
expected/recorded worker counts, unlisted workers from an incomplete manifest,
record count, captured-file SHA-256, truncated final line and whether a complete
`run_finished` record exists. Historical task outcome, task state, stop evidence,
result hash and local process ID remain historical evidence, not a live probe.
A recorded PID may have been reused and is never used for cleanup or reattachment.

The reader pins a plain single-link file through O_PATH before reading its inode.
Symlinks, hard links, devices, FIFO inputs and observed concurrent mutations are
refused. Bounds are 16 MiB per journal, 64 KiB per line, 100,000 complete records,
128 workers, 64-byte run/worker IDs and 256-byte task/context IDs. Schema, contiguous
sequence, monotonic elapsed time, worker manifest/lifecycle, outcome and completion
counts must agree. Unsupported schemas/events or malformed complete lines fail
closed. The native run-level `cancel_all` event is accepted only for a fully
recorded native worker manifest; worker-scoped and A2A variants are refused.
A trailing line without a newline is ignored and explicitly flagged,
even if it looks like valid JSON; it is not a durable binding or terminal record.
No raw prompts, results, credentials or diagnostic text are recovered from hashes.
The file hash binds the captured bytes, including an ignored tail; it does not
sign/authenticate a journal or prove the machine survived a power loss.

## Recover completed worker artifacts offline

New A2A runs publish `worker-<worker-id>.json` beside `transcript.jsonl` before
recording each worker's durable `finished` event. For the built-in CLI, worker 1
is `<directory>/run/worker-worker-1.json`. Each versioned checkpoint contains the
run ID and complete worker report, including results and review proposals. The
finished journal record binds its exact bytes with `checkpointSha256`. The
hash-only journal still contains no result text or credential.

```sh
./target/debug/oxa orchestrate-inspect \
  --journal ./run-directory/run/transcript.jsonl \
  --worker worker-1 --checkpoint ./run-directory/run/worker-worker-1.json \
  > recovered-worker.json
```

This explicit read-only operation prints a one-worker A2A run report. It can feed
`orchestrate-review --report recovered-worker.json` with the original approved
source scope and the usual explicit selection/approval gates. Recovering a report
is not approval to apply it. Partial/canceled outcomes retain their original
meaning and remain ineligible for completed-proposal review.

The library equivalent is `InspectedRun::load_checkpoint(worker, path)`. It reads
only the explicitly selected bounded plain single-link file, pins the inode and
checks for observed concurrent mutation. It verifies the exact captured-file
hash, schema, run/worker identity, recorded outcome/stop and task/context/state
and result evidence. It performs no discovery, network request, process action,
file write or prompt replay. It does not require the original agent to survive.

Publication uses a 0600 pending file, a 64 MiB per-worker output cap, file sync,
no-replace rename and parent sync in the protected private run directory. The
checkpoint's exact bytes are hashed while writing. Publication precedes the
journal finish; journal finish precedes the whole-run report. A crash after a
worker finish can preserve that worker even if later work never finishes. A crash
between publication and the complete journal line can leave an orphan file;
a pending file, orphan file or unterminated finish record is never a recovery
binding. Storage failure stops the scheduler through its owned cleanup path,
without overwriting an existing result, recording a false worker finish or
restarting inference. File-system I/O/sync latency remains outside a hard real-time
guarantee, as for the audit journal.

Old journals without checkpoint hashes remain inspectable and can use explicit
agent reconciliation. They cannot load an unbound local checkpoint. Checksums
prove consistency with the selected journal, not authenticity against an operator
or root modifying both files, nor a universal power-loss guarantee. Checkpoints
contain delegated result/source text and must stay private. There is still no
automatic directory scan, active-stream checkpoint, queued-plan resume or takeover
of an interrupted controller.

## Explicit A2A reconciliation API

`recovery::InspectedRun::reconcile` can query a known task on an agent that outlived
its controller. The operator must register the ORIGINAL trusted local-only agent
instance/capability with `LocalAgent` and choose `Action::Observe` or `Action::Cancel`.
The API accepts explicit time and aggregate response-wire bounds, at most five
seconds and 16 MiB. It never reads a capability from a journal, environment,
configuration or discovered card. The CLI deliberately accepts no transport
credentials and performs no network reconciliation.

New scheduler journals record SHA-256 of the registered endpoint URL in each
queued worker and the observed task state at finish. No URL capability, provider
credential or model request enters the journal. An endpoint hash prevents an
accidental route change; it is not server authentication. The operator must ensure
that this is the same agent instance, with stable non-reused task identities and
the original explicit capability. A reused loopback port alone is insufficient.
Older journals without an endpoint binding remain inspectable but cannot reconcile.
Local RPC journals cannot be used to reattach a process.

Reconciliation validates the original route before network access, discovers its
card, then queries the durable task ID and checks its context. It never invokes
`message/send`, starts a new task or retries the initial prompt. A task that was
accepted before its identity reached a complete journal record remains unknown:
there is no safe automatic replay. A missing task is `not_found`, never permission
to recreate it. A new unrelated agent instance cannot restore a lost native task.

Observe returns the last valid task snapshot. Cancel first queries and verifies
identity, then requests cancellation only if the task is still nonterminal. If a
cancel response is lost, one additional query can confirm the stop within the
SAME absolute deadline and aggregate wire allowance. The operator explicitly
chooses cancellation; inspection alone never cancels anything. Refusal, timeout,
response limits, context mismatch and invalid replies leave stop unconfirmed.
Where available, a recorded terminal task state and artifact hash must still agree;
a completed task cannot regress to working or silently substitute different results.
An artifact hash recorded while stop was unconfirmed is partial evidence: a later
terminal snapshot may contain further results without changing the original outcome.

`remote_stop: confirmed` means the original A2A agent returned a validated terminal
state. It is not proof about an arbitrary remote operating-system process.
`recorded_outcome` is preserved separately: observing a later completed task does
not turn an original deadline/budget failure into successful execution. Recovered
artifacts are untrusted result data; no commands or patches execute automatically.
Reconciliation does not modify the original journal or silently persist results.
Callers decide where to save approved returned data.

The integrated `oxa orchestrate` command owns its ephemeral native server and
capability in the controller process. After that process dies, there may be no
surviving server to query; bubblewrap parent-death cleanup stops its workers.
The inspector still reports missing terminal evidence honestly. It never restarts
that server or treats a new server as a continuation of the old one.

The CLI saves its final report to `<directory>/report.json` after owned cleanup
and before stdout, including partial outcomes. See [CLI.md](CLI.md#saved-final-report)
for publication and failure semantics. This preserves completed results when a
stdout consumer closes, but cannot recover a controller killed before publication.
Pending temporary output is not treated as a completed report.

This is recovery inspection and explicit task reconciliation, not scheduler resume.
Persistent authenticated agent registration, active-task artifact checkpoints,
automatic reattachment/resubscription, queued-plan resume, concurrent-controller
ownership and recovery after machine failure remain unfinished. No cloud/provider
route or paid-call authorization is introduced.

## Validation

```sh
cargo test --locked -p oxa-orchestrator --test recovery
cargo test --locked -p oxa-orchestrator --test real_workers
```

Owned loopback backends verify completed-result recovery without a second send,
interrupted bindings, explicit cancellation with a lost response, missing task IDs,
route/context mismatch, refused cancellation, deadline/wire limits, terminal result
mutation/regression, journal corruption and link refusal. The actual CLI crash
scenario kills its owned controller during inference, checks worker exit through
a PID descriptor pinned while ownership is known, then inspects the surviving
journal without changing it, probing inference or claiming a terminal outcome.
The feature-built binary smoke check also runs that scenario.
