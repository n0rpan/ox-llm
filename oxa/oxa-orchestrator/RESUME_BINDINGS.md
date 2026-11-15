# Original plan and clock bindings

The default-off `core` A2A scheduler now records versioned run/worker bindings
for future queued recovery. This slice adds metadata and deadline enforcement;
it does not consume a queue, publish a resume receipt or launch a resumed run.

A run binding fixes its original kernel boot-identity hash, CLOCK_BOOTTIME start
and deadline, parallelism, response reservation, journal bound, request timeout,
poll interval, cancellation grace and raw-result retention consent. Boot time
includes suspend. `RunBinding::remaining` subtracts current time from the original
deadline; a different boot, regressed clock or expired allowance is rejected.
No fresh allowance is minted when the caller reads the binding. The scheduler
checks the boot deadline during control/admission loops as well as its bounded
Instant deadline; cleanup retains its separate, already-configured grace.

Each worker binding fixes role/task/endpoint hashes, wall time, response bytes
and optional observed-token cap. It contains no raw role/task text or agent
capability. Endpoint hashes prevent accidental route changes and do not replace
authentication or the operator's local-only assertion. These are observed usage
and incoming-byte limits, not a pre-request provider-spend guarantee.

Inspection validates binding versions, bounds and policy, requires complete
worker bindings when a run binding exists, checks them against queued manifest
hashes/reservations, and checks aggregate reservations against the original run.
Unknown fields and conflicting consent/parallelism are refused. Legacy journals
without bindings remain inspectable and reconcilable, but lack the evidence
needed for future queue resume. Native inspection still rejects this transport.

Local acceptance: 110 ordinary crate tests pass, including boot regression,
expiry/reboot and downtime arithmetic, fingerprints changing for each original
input/route/allowance, serialization roundtrip without raw input/route text,
13 malformed or widened metadata cases, and all actual scheduler journal and
checkpoint fixtures. No model, shared gateway, paid request or UI is involved.
Queue consumption, exact-plan resubmission and lineage remain unfinished work.
