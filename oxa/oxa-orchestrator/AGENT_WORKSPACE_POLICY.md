# Fixed workspace policy on an A2A native host

With default-off `core`, `RpcAgentConfig.workspace` optionally fixes an immutable
operator-approved grant for the host. `None` retains the tool-free profile.
Read-only and editing grants derive the native mode/tools from that grant.
An editing Git grant creates one controller-owned private pool inside the new
host directory; the existing pool/task limits remain in force.

Every admitted task gets a fresh copy under a controller-generated task name.
Preparation, native work and post-reap collection use the same supervisor
lifecycle, deadline, cancellation and lane ownership as direct workspace runs.
Incoming role/model/mode/tool/workspace requests remain task data and cannot
replace this configured policy. No grant or credential is discovered from a
card, request body or worker result. Task input never changes host budgets.

The backend waits for the supervisor to return before publishing terminal state
and artifacts together under its state lock. A successfully collected review
becomes a versioned `oxa.workspace.review.v1` data artifact. It contains the
bounded before/after contents, deterministic diff and snapshot digest; raw
worker text is not parsed as a review or local authority. Unknown cleanup stays
nonterminal/unknown. Cancellation skips review collection and produces no review
artifact. Other failed/truncated outcomes remain unsuccessful even if they
retain diagnostic proposal data.

The controller's existing A2A scheduler can collect that artifact into an
explicitly retained report/checkpoint. Saved-report selection still requires
reported completion and known stop; candidate preparation independently verifies
contents/scope against the caller's snapshot, and staging requires the exact
prepared digest. No source checkout is modified. A read-only task may complete
with an empty review, which is not a changed-file candidate. Returned data does
not itself authorize publication or mark a parent goal complete.

Source paths and Git metadata remain outside the worker mount. The worker gets
only the private copy, with read-only enforcement and Git masking where selected.
Native process-output/token limits, review bounds and transport response
reservations remain separate. No provider-spend proof, per-response allowance
change, CLI/TUI hook or production registry bridge is added here.

Local acceptance: 141 ordinary crate tests pass. End-to-end authenticated
loopback tests drive actual isolated native mock workers through the A2A
scheduler, retained reports, review verification and exact candidate staging.
Read-only, editing and Git-backed cases reject task-level policy overrides,
include shutdown-time edits only after reap, and leave the source unchanged.
Git-backed cancellation publishes no review, confirms stopped cleanup, and has
no delayed copy writes after acknowledgement. No real provider, shared gateway
or paid request is involved.
