# Model-free supervisor demo log

This is the local RPC foundation, not the final orchestration CLI demo. Every
inference request uses an owned loopback fake engine. No installed model or
service is contacted. The worker fixture runs the actual oxa CLI/RPC code in a
private bubblewrap sandbox; dedicated hostile fixtures exercise failure paths.

Reproduce from `oxa/`:

```sh
cargo test --locked -p oxa-orchestrator --test real_workers
```

Observed successful run:

```text
test parallel roles and fan-in ... ok
test queued cancellation and preflight ... ok
test deadlines and caps ... ok
test live cancellation and steering ... ok
test partial failure and sandbox isolation ... ok
test audit failure cleanup ... ok
test hostile RPC workers and run deadline ... ok
test inherited modes and task command wrapping ... ok
```

The first scenario verifies three overlapping workers, distinct role prompts,
all-required fan-in, attributed results and process reaping. The remaining
scenarios verify partial failure, observed token/output caps, cancellation,
steering during inference, journal failure cleanup, private file/environment
boundaries, denied permission dialogs, attempted mode changes, a descendant
holding pipes and a task starting with `!` staying data. All three permission
modes are passed through and verified. Assertions run inside the suite; this
log alone is not proof of broader protocol or production readiness.
