# Explicit local A2A worker scheduling

Preparation slice from #88 for #282, behind default-off `core`. `a2a_run::run_a2a`
accepts fixed worker IDs, role/task data, explicit verified-local agent clients,
per-worker time/response allowances, and an optional observed-token cap. Agent
cards/results cannot grant local filesystem, process or provider authority.
An operator's local-only assertion is necessary; a loopback address alone does
not prove that the agent cannot forward to paid inference.

The controller factory provides a bounded 32-command queue and an independent
sticky cancellation flag. Steering acknowledgements report reconciliation of a
remote task response; a missing acknowledgement is uncertain, not a retry grant.
A failed initial send is never replayed. Paused/auth-required tasks keep their
deadlines. All incoming discovery, polling and cleanup bytes share explicit
reservations, with a reserved cleanup portion and transfer of unspent read bytes.
Cancellation is confirmed only by a matching-context terminal remote snapshot;
this is a protocol assertion, not proof of provider billing or local process reap.
Unknown task identities and refused cleanup retain an unconfirmed stop outcome.
Observed usage caps require the native usage extension and final valid usage.

Results remain caller-owned data. Only hashes/status/identity/accounting enter
the private create-only journal. Explicit `persist_results` consent now enables
private worker checkpoints; see [WORKER_CHECKPOINTS.md](WORKER_CHECKPOINTS.md).
Passive remote journal/checkpoint inspection is now in
[A2A_INSPECTION.md](A2A_INSPECTION.md). Reconciliation/resume, registration loading
and CLI hooks remain later slices. Native inspection still rejects this transport.
Local acceptance: 88 ordinary crate tests pass, including eight scheduler tests
using authenticated loopback mocks, partial fan-in, lost IDs/no replay, direct
messages, cleanup budget transfer, paused-task deadlines, slow steering and
public control saturation/acknowledgement. No model or shared gateway is used.
