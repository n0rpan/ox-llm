# Explicit original-agent reconciliation

`a2a_recovery::ExclusiveRun::reconcile` adds an opt-in operation to the existing
default-off `core` API. Passive journal/checkpoint inspection still performs no
network I/O. Reconciliation requires a claimed inactive cooperative journal and
an operator-supplied `LocalAgent` for the original instance/capability; a journal
or discovered card cannot supply credentials or widen permissions.

The worker must have a complete durable task/context binding. Its recorded
endpoint hash must match before any request is made. That hash only prevents
accidental routing changes: authentication still depends on the caller providing
the original agent capability. A loopback endpoint is not itself proof that the
agent cannot forward to paid inference; the local-only assertion remains the
operator's responsibility.

Only discovery, task get and (for explicit `Action::Cancel`) task cancel are
used. No message/send, input replay, worker launch, local tool, journal append,
checkpoint replacement, task retry or recorded-PID signaling is performed.
A lost identity, wrong route, context mismatch, missing task or rejected response
never authorizes another submission.

All requests share one absolute timeout (at most five seconds) and one aggregate
incoming response allowance (1 KiB to 16 MiB). If cancellation loses its response,
one get may observe the result within those same bounds. A matching-context
terminal snapshot confirms the protocol-level remote stop; it is not independent
proof of process reap or provider billing. Refused cleanup or exhausted limits
retain uncertainty. A terminal observation is checked against already-recorded
terminal state and artifact digest; conflicting results are rejected.

The returned `Reconciliation` binds the journal digest and worker, preserves the
historical outcome, and separately reports the new disposition, stop assertion,
observed task and bytes. A late terminal result cannot rewrite an original
budget/deadline failure into success. Raw result content remains untrusted data,
not authority to edit source or mark a parent goal complete. Keep the exclusive
journal guard alive throughout the operation. Queued resume and lineage remain
separate unfinished capabilities.

Local acceptance: 107 ordinary crate tests pass, including eight authenticated
loopback reconciliation tests. They cover explicit observation/cancellation,
lost cancel response, no requests before admission checks, wrong capability and
context, missing task, refused/slow/oversized peers, cancellation-context change,
unchanged terminal evidence and late completion preserving an earlier failure.
Every fixture asserts zero input sends. The journal remains unchanged. No real
provider, shared gateway, paid request, native registry or TUI is involved.
