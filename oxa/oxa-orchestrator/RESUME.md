# Explicit queued-plan resume

`resume::resume_queued` can continue an interrupted A2A scheduler's never-started
jobs while preserving its original inputs and allowances. It is a library API
for explicitly registered local/nonbillable agents that outlive the scheduler.
The explicit [foreground host](AGENT_HOST.md) can survive controller exits.
The integrated orchestrate CLI still owns an ephemeral server/capability; restarting that CLI
cannot recreate its original agent instance or recover a lost task.

```rust,ignore
let owner = recovery::ExclusiveRun::claim(&original_journal)?;
let report = resume::resume_queued(
    &owner,
    &new_child_directory,
    original_queued_specs, // caller supplies the original prompts and LocalAgents
    controls,
    cancellation,
    |status| render(status),
)?;
```

Keep the exclusive owner alive throughout the operation. Inspection alone never
resumes anything. This API performs an explicit write and launch operation; the
read-only `orchestrate-inspect` command still performs neither.

## Eligibility and identity

The original journal must declare cooperative inode locking, have a complete
worker manifest, have no torn tail, and lack a completed run record. Every job
must be either never started (`queued`) or durably finished with acceptable stop
evidence. Any `starting`/bound task, unknown initial-send acceptance or unconfirmed
stop refuses the entire operation. Those tasks require reconciliation; they are
never sent again by queued resume. Finished jobs require their original validated
local result checkpoints. Enable `--save-results` / `persist_results` originally
if later recovery must preserve collected results. A completely unstarted plan
can resume without raw-result persistence, and keeps that choice off.

The caller supplies exactly all never-started jobs with their original IDs,
roles, task text, worker wall-time caps, incoming-wire caps, reported-usage limits
and registered endpoint. New journals record versioned typed bindings for these
fields (hashes for text and endpoint). Changed inputs, changed routes, duplicate
IDs, substituted finished jobs, missing jobs and changed limits are refused before
any new directory, receipt or network request. No raw prompt or capability is
read from a journal or saved plan file.

The caller must re-register the ORIGINAL trusted local agent instance/capability.
An endpoint hash is a routing check, not authentication or proof against port
reuse. The capability remains caller-owned and separate from the journal. Agent
cards/results cannot select routes, models, tools or grants; original backend
configuration and parent permission checks remain in force.

## Budgets survive downtime

Run bindings capture a hashed kernel boot identity, a `CLOCK_BOOTTIME` start and
absolute deadline, parallelism, original response reservation, journal bound,
request/poll/cancel timings and result-retention choice. Remaining time is the
original deadline minus the current boot clock. Downtime and machine suspend
consume it. Reboot, clock regression, expiry or unsupported bindings refuse
resume; system wall-clock/NTP changes cannot grant a fresh allowance.

The new scheduler rechecks the original deadline after receipt publication and
again on entry, using a conservative monotonic time bracket. Child scheduling
and cleanup use the established bounded supervisor. Per-worker limits are
unchanged; only never-used queued response reservations enter the child run.
Completed workers' usage/results remain attributed and unchanged, including
unknown usage. This is a free local-agent profile: reported token counts still
are not a hard monetary cap or permission for paid/cloud routing. File-system
I/O/sync and OS scheduling do not become hard real-time guarantees.

## One-time parent consumption

Under the original journal's exclusive owner, the controller publishes a private
0600 `resume.json` receipt in the pinned original run directory. It contains only
metadata: parent journal hash, parent/child run IDs, destination-path hash, queued
worker IDs and inherited limits. Publication uses a bounded pending file, file
sync, a no-replace rename and parent sync. The original journal bytes are never
appended, rewritten or truncated by this operation.

The receipt must be durable before any child send. An existing receipt or pending
entry refuses another attempt. A storage failure or controller crash after the
receipt, even before a single child send, leaves the parent queue consumed. Do
not delete that receipt to retry: an apparently failed send can have been accepted.
The operator instead inspects/reconciles the explicitly selected child journal.
A failure before a usable child journal exists can leave the continuation unknown;
this profile chooses no duplicate launches over an automatic retry. Same-UID/root
are trusted; checksums/advisory locks are not tamper-proof against an operator
changing or deleting both journal and receipt.

The child uses a distinct run ID and a fresh private directory. Its journal keeps
the original boot-clock start/deadline and the queued subset's unused reservation.
On success, `resume_queued` returns an A2A report for its selected parent manifest,
combining recovered finished results with new queued outcomes. A child is a
separate manifest, not an implicit claim that every ancestor completed. For further
child recovery, [explicit lineage collection](LINEAGE.md) can verify an operator-
selected chain and restore full root fan-in. Neither API scans for directories.

## Current boundaries and validation

Cross-host/reboot agent identity, automatic bound-task reattachment,
reconciliation that authorizes continuing a mixed ambiguous/queued plan,
automatic lineage discovery/collection, cross-host/distributed ownership,
CLI takeover and recovery across machine reboot remain unfinished. No cloud run,
checkout application, automatic merge or review-gate bypass is introduced.

Owned loopback tests verify original result preservation, exactly one send for a
never-started job, no resend for a finished job, input/route/limit substitution
refusal, original deadline and reservation inheritance, downtime expiration,
ambiguous-start refusal and no parent replay after a child fails before sending.
A model-free end-to-end scenario unwinds the scheduler after its first actual
sandboxed RPC worker finishes, reclaims its journal while the original agent
survives, resumes only the untouched second worker and verifies both workers are
reaped. Clock tests cover reboot, regression and expiry without changing the host
clock. No installed model, provider credential or production service is used.
