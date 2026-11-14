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
test native Git worktree isolation ... ok
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
test native cancellation journal inspection ... ok
test orchestrate CLI Git worktrees ... ok
test orchestrate CLI stalled stderr ... ok
test orchestrate CLI durable report with closed stdout ... ok
test orchestrate CLI completed worker checkpoint after crash ... ok
test orchestrate CLI structural input limits ... ok
test orchestrate CLI protected run namespace ... ok
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

Native cancellation inspection now also exercises the actual `Control::CancelAll`
producer and the active CLI SIGINT/cancellation-flag producer. Both completed
canceled journals and copies interrupted immediately after their cancellation
record remain inspectable; source journal bytes are unchanged. Worker-scoped,
A2A and incomplete-manifest cancellation records are rejected.
The native Git worktree scenario verifies two registered detached worktrees at a
shared approved base while the actual workers see only masked read-only Git
metadata. The CLI worktree scenario verifies shared controller metadata across
A2A tasks, refused metadata writes, review proposals, unchanged source and ignored
ambient Git configuration/object-store injection. See [GIT_WORKTREES.md](GIT_WORKTREES.md).
The stalled-stderr case fills a blocking pipe before starting the command and
keeps its reader open without draining it. The one-second run deadline still
produces a partial JSON report, reaps the owned workers and closes the observed
owned listener. The caller's stderr flags stay unchanged. The actual feature-built
binary smoke check exercises the same case.
Structural input validation uses the same bounds as output collection. Unit tests
materialize and collect unchanged snapshots at 16 directory levels and 128 total
entries. The CLI rejects 17-level and 129-entry input layouts before creating run
state or probing inference; eligible new-file scope is checked too.
Candidate publication additionally rejects a group-writable destination parent
before staging state is created. Namespace tests also reject a replaceable
ancestor and symlink aliases, while private-parent publication still produces the
exact approved candidate hash. No existing permissions are changed by staging.

The closed-stdout scenario discards the pipe reader before inference finishes,
then verifies exit 1, a complete saved report with terminal task results, no
pending report and reaped native workers. The success scenario checks byte-for-byte
equality between saved report and stdout, including escaped terminal controls.

The worker-checkpoint crash scenario kills the actual controller after worker 1's
complete checkpoint binding and before worker 2 finishes. With no final report,
it recovers worker 1 through both the library and the read-only CLI, preserves the
original journal bytes, and refuses worker 2 using worker 1's file. Recovery tests
also reject changed bytes, links and orphan/torn-finish bindings.
