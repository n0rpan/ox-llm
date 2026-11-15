# One-time recovery of never-started A2A work

With default-off `core`, `resume::resume_queued` accepts an exclusive claim on an
inactive A2A journal, a new destination, a complete re-supplied set of queued
worker specifications, bounded controls and a trusted nonblocking observer.
It does not discover inputs or credentials from model output or checkpoints.

Eligibility is deliberately narrow: a complete manifest, no incomplete tail,
no recorded run completion, and valid original boot/plan bindings are required.
Every worker must be either never started or finished with unambiguous terminal
evidence and a verified retained checkpoint. Started/bound, stop-unconfirmed or
otherwise ambiguous workers make this parent ineligible. Observation alone does
not rewrite those historical records. Legacy journals without bindings and
expired or rebooted budgets cannot resume.

The caller supplies exactly every queued ID, with unchanged role/task/endpoint
hashes, per-worker wall/response/token allowances and original agent credentials.
The API enforces the fingerprinted set and policies; the caller may order that
set. Completed results load through the captured private parent directory inode,
not a reconstructed path. Existing results are retained without another send;
failed outcomes remain failed in the combined report.

After all preflight checks, a bounded mode-0600 `resume.json` is atomically
published without replacement in the parent directory. It binds the original
journal digest, child run ID, destination hash, queued IDs and inherited limits.
It is synced before any child launch. The original journal is unchanged. A
pre-existing final or pending receipt blocks the operation; no file is silently
removed to retry. A sync failure can leave a complete receipt and no child run.
That is uncertainty to inspect, never permission to submit the parent queue again.

The child reuses the original boot start/deadline and policies. Controller
exit/downtime/suspend count against that deadline. Its response reservation is
only the queued workers' original allocations. Time is checked again after
receipt I/O. A child error or controller crash intentionally leaves the queue
consumed, even if no send was observed. The API guarantees no automatic replay;
it does not guarantee delivery, remote process cleanup, provider billing or
recovery from arbitrary operator copies/deletion of private state.

The exclusive claim and all checkpoint/receipt I/O remain bound to validated
private inodes, including after a trusted rename. Same-UID/root actors remain
inside the operator trust boundary. No CLI, TUI, source apply, Git publication,
registry change or paid route is enabled by this slice. Multi-generation lineage
inspection remains the next capability.

Local acceptance: 116 ordinary crate tests pass, including six queue-resume
regressions against actual authenticated loopback agents. They cover exact-plan
and queue-cardinality refusal, checkpoint and receipt obstruction, renamed-parent
ownership, inherited deadline/downtime, ambiguous starts, child crash after queue
consumption, no repeat send for prior results and preservation of retained
failures. Existing private-reader, atomic-publication and short-storage tests
remain active. No provider/shared-gateway or paid request is involved.
