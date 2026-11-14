# Model-free supervisor demo log

This is the local RPC/A2A orchestration foundation and opt-in CLI demo. Every
inference request uses an owned loopback fake engine. No installed model or
service is contacted. The worker fixture runs the actual oxa CLI/RPC code in a
private bubblewrap sandbox; dedicated hostile fixtures exercise failure paths.

Reproduce from `oxa/`:

```sh
cargo test --locked -p oxa-orchestrator --test real_workers
```

Observed successful run:

```text
test approved workspace edits and read-only policy ... ok
test A2A workspace review round trip ... ok
test parallel roles and fan-in ... ok
test queued cancellation and preflight ... ok
test deadlines and caps ... ok
test live cancellation and steering ... ok
test partial failure and sandbox isolation ... ok
test audit failure cleanup ... ok
test hostile RPC workers and run deadline ... ok
test inherited modes and task command wrapping ... ok
test A2A RPC bridge streaming, steering and deduplication ... ok
test A2A cancellation, resubscription and shutdown ... ok
test cancellation bypasses full control queues ... ok
test A2A scheduler coordinates real RPC workers ... ok
test A2A scheduler steering, cancellation and wire caps ... ok
test orchestrate CLI success and inert validation ... ok
test orchestrate CLI signal cleanup ... ok
test orchestrate CLI approved files ... ok
test orchestrate CLI crash inspection ... ok
```

The first scenario verifies three overlapping workers, distinct role prompts,
all-required fan-in, attributed results and process reaping. The remaining
scenarios verify partial failure, observed token/output caps, cancellation,
steering during inference, journal failure cleanup, private file/environment
boundaries, denied permission dialogs, attempted mode changes, a descendant
holding pipes and a task starting with `!` staying data. All three permission
modes are passed through and verified. Assertions run inside the suite; this
log alone is not proof of broader protocol or production readiness.

The A2A scenarios start an authenticated loopback server, send tasks through
the A2A client, steer actual RPC workers while inference is running and stream
their artifacts/usage. They also disconnect and resubscribe to a task without
relaunching it, cancel while another HTTP connection is streaming, and verify
worker cleanup when the server shuts down with a stalled connection.

The scheduler scenario sends three role-specific jobs over A2A with two active
workers, observes actual overlap at the fake inference engine, and verifies
attributed terminal tasks, usage and process cleanup. Separate scheduler tests
cover direct replies, failed peers, paused deadlines, unconfirmed stops, missing
task IDs, slow steering calls and cleanup responses larger than the initially
reserved wire allowance. Reproduce those with:

```sh
cargo test --locked -p oxa-orchestrator --test a2a_scheduler
```

The CLI scenarios also pass through an actual `oxa` binary built with
`--features orchestrator`. They verify two worker results, live stderr status,
JSON-safe terminal controls, no engine access for help/rejected options,
refusal to overwrite an existing directory, and SIGINT exit 130 after worker
reaping. The default build excludes the orchestration dependency; enabling the
feature leaves base help byte-identical. See [CLI.md](CLI.md) for reproduction.

The workspace scenarios read and write an explicitly approved private copy with
real oxa tools, verify its review bundle locally and through A2A fan-in, check
that source contents stay unchanged, and reject read-only writes and editing
grants under plan mode. See [WORKSPACES.md](WORKSPACES.md) for API boundaries.

The approved-files CLI scenario additionally saves the real A2A report, previews
its selected proposal without contacting inference, rejects a wrong approval,
stages the exact approved candidate and checks its receipt and unchanged source.
It rejects existing destinations and stale source, and reports overlapping paths
without choosing a worker automatically. The feature-built binary smoke check
runs the same end-to-end review command. See [REVIEW.md](REVIEW.md).

The crash-inspection scenario forcibly terminates its owned controller while a
real RPC worker is running, verifies the worker exits through a pinned PID
handle, and inspects the surviving journal through the CLI. It preserves unknown
terminal evidence, leaves journal bytes unchanged and makes no new inference
requests. Separate recovery tests reconcile known original A2A tasks without
resending input. See [RECOVERY.md](RECOVERY.md).
