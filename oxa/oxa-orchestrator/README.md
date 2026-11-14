# oxa-orchestrator

Work in progress for milestone 3 of the
[orchestrator design](../docs/design/a2a-orchestrator.md).
This library has no startup side effects and is not linked into the normal oxa
binary. There is no default orchestration command or network listener.

## Executable profile

`run` validates a complete plan and its aggregate reservations before launching
anything. It schedules up to the configured parallelism, assigns each oxa RPC
worker a separate role and task, accepts bounded steering/cancellation controls,
and returns every outcome and result. Fan-in uses all-required success; partial
results remain available. Results embed tagged A2A Task objects, scoped by run
and worker identity. Unknown execution outcomes are not success.

The first profile is **local inference with no tools**. An operator must register
a verified local-only endpoint through `LocalInference`; the loopback address
check alone does not prove that an oxc proxy cannot forward to the cloud. This
API is a trusted operator boundary, not a declaration an agent may make about
itself. No cloud adapter, provider credential or paid request is supported.

Each worker is a trusted, explicitly chosen oxa executable inside bubblewrap:

- New mount, PID and user namespaces; private work directory, HOME, temporary
  files, agent state and XDG paths. Only system runtime files under `/usr`,
  `/lib` and `/lib64`, and the chosen executable, are mounted read-only.
- Cleared environment, offline enabled, no tools, MCP, extensions, skills,
  context discovery, project approval or persistent sessions. The configured
  parent permission mode is passed unchanged and verified before prompting.
- The network namespace is shared to reach the registered loopback inference
  server. Egress restrictions rely on the trusted oxa offline checks; tools and
  dynamically loaded code are disabled. This is not a kernel-enforced network
  destination allowlist. Stronger endpoint isolation remains work to do.
- The assigned task and steering text are wrapped as data so a leading `!` or
  slash command cannot invoke a user command or change permission settings.
- Nonblocking stdin/stdout/stderr, bounded writes, line lengths, event counts
  and total accepted output. No reader thread or unbounded queue can remain
  stuck on a descendant's pipe. Per-poll work is bounded for fairness.
- Worker/run deadlines, abort with bounded grace, process-group termination,
  PID-namespace cleanup and reaping on success, failure and drop. Missing
  bubblewrap fails closed. There is no unsandboxed fallback.

Wall-clock enforcement has the supervisor's 2 ms polling granularity plus OS
scheduling/termination overhead; the trusted status callback must not block.
The remaining deadline bounds shutdown grace. Output caps bound accepted data;
an engine may already have generated more output before the supervisor stops
it. Token caps likewise stop on **reported** usage and cannot prevent a request
from overshooting before that report. They are not hard monetary ceilings.
Only local, non-billable routes are eligible until provider-side ceilings exist.

## Audit and results

Each run creates a new private directory and a bounded `transcript.jsonl`
(0600) with ordered worker transitions, role/task hashes, usage, decisions,
process IDs and result hashes. Raw prompts, credentials, diagnostics and model
output do not enter that journal. Results are returned in memory; callers must
decide whether and where to persist approved content. Audit failure stops the
run and drops all owned workers. Existing run directories are never reused or
replayed. Crash reconciliation is not implemented yet; a new run must not be
treated as resuming an old task.

The role/task content is explicit delegation input. Do not supply secrets or
private parent context. No parent transcript, filesystem grant or environment
is automatically forwarded. Returned text is data and is never executed or
applied to the parent workspace.

## Verification

Run `cargo test --locked -p oxa-orchestrator` from `oxa/` with working bubblewrap.
The harness-free integration executable doubles as an oxa worker by delegating
to `oxa_cli::run` when launched in RPC mode. All inference comes from owned
loopback fake engines. Tests cover real concurrent workers and role assignment,
steering, cancellation, partial failures, deadlines, output/usage caps, private
filesystem/environment boundaries, audit failure, hostile records and process
cleanup. Test-only hostile RPC fixtures also check that acknowledgment without
a final assistant response cannot produce success.

No installed runtime, model, oxc service or another machine is used. The new
workspace member and package lock entry are the only existing-file changes;
manifest registration is subject to architect/coordinator review. All external
dependencies already exist in the workspace. The legacy `oxa-rpc::RpcClient`
remains unchanged: its inherited environment, unbounded channels/line reads and
single-child cleanup do not provide this supervisor's guarantees.

## Remaining milestone work

Remote A2A scheduling and an oxa RPC-to-A2A server adapter, streamed artifact
assembly, stronger inference-network isolation, hard provider request/spend
limits, approved file inputs/worktrees and patch review, restart reconciliation,
and opt-in CLI/status UX remain required. This working local profile is a
foundation, not a claim that the full orchestration brief is complete.
