# A2A orchestration in oxa

Status: proposed; milestone 1, coordinator review pending. No runtime change.

## Scope and protocol decision

An opted-in orchestrator starts workers with separate roles, steers their tasks,
collects bounded results, and presents their evidence and proposed patches for
review. Workers do not merge their own work. Keep the implementation in new
`oxa-a2a` and `oxa-orchestrator` crates, using the existing
[RPC interface](../pages/rpc.md) for child oxa processes. Existing core,
interactive, configuration, sandbox and oxc code need no refactor.

Pin the initial wire profile to [A2A 0.3.0](https://a2a-protocol.org/v0.3.0/specification/),
JSON-RPC 2.0 over HTTP with SSE for streaming. This is an explicit compatibility
target, not a claim to track the newest specification. Reject incompatible
protocol versions. Implement discovery, `message/send`, `message/stream`,
`tasks/get`, `tasks/cancel` and `tasks/resubscribe`; expose unsupported methods
as protocol errors. Do not advertise push notifications or extended cards until
implemented. A2A connects agents; MCP remains the interface for tools.

[T3 Code](https://github.com/pingdotgg/t3code#readme) provides useful ideas:
separate agent threads, worktrees per thread, a common control surface, and
reviewing a diff before committing or publishing it. Reimplement these ideas in
Rust. Fan-out scheduling and budget enforcement below are oxa design decisions,
not claims about T3's implementation.

## Placement and activation

`oxa-a2a` owns wire types, validation, bounded HTTP/SSE transport and a loopback
mock server. `oxa-orchestrator` owns policies, scheduling, child supervision,
RPC adaptation, accounting and transcripts. Neither crate imports interactive
UI code. Use the workspace's synchronous style, bounded channels and worker
threads; no async runtime is required.

Initially provide an explicitly invoked companion binary for model-free demos.
Milestone 4 adds `oxa orchestrate <goal>` behind a Cargo feature that is absent
from default features. The CLI hook only delegates to the new crate; its exact
lines and manifest changes must be listed in that PR. No startup listener,
discovery probe or subprocess exists when disabled. New manifests must account
for the architect-owned manifest warning in `oxa/Cargo.toml`; request review of
workspace registration and dependencies separately from ordinary source edits.

## Agent cards and transport

Serve cards at `/.well-known/agent-card.json`. Include protocol version, name,
description, URL, preferred transport, capabilities, default input/output modes
and skills. Use opaque stable skill IDs and no private paths or credentials.
Cards describe capabilities, never grants. Initial parts are bounded text and
structured data; file URI dereferencing and executable artifacts are disabled.

The operator registers an endpoint and its permitted skills out of band. A card
cannot redirect traffic to another origin or broaden that registration. Resolve
and validate destinations before connecting, disable redirects and ambient
proxies, and recheck every new connection. Local tests bind literal loopback on
an ephemeral port. Non-loopback deployments require TLS certificate validation
and a separately configured endpoint credential. No unauthenticated listener on
wildcard interfaces. A loopback listener still needs an ephemeral capability
token supplied privately to its launched client, never included in its card.

Use distinct transport credentials per agent. They terminate at the transport
layer and never enter prompts, results or transcripts. Reject URL userinfo,
unexpected auth challenges and credentials requested in task content. Initial
HTTP adapters support operator-provided bearer credentials only; unsupported
security schemes fail closed rather than silently downgrading authentication.

## Task lifecycle, streaming and steering

Each run has a local run ID, each worker a local worker ID, and each wire task
its server-issued task ID and context ID. JSON-RPC request IDs only correlate
requests. Bind received task/context IDs to the authenticated worker; reject
events claiming another worker's task. A send may return a Message directly,
which is a completed response without inventing a server task ID.

Record A2A states `submitted`, `working`, `input-required`, `auth-required`,
`completed`, `failed`, `canceled`, `rejected` and `unknown`. Terminal task states
are completed, failed, canceled and rejected. Input/auth required pause useful
work but do not stop wall-clock accounting. Unknown or lost observation is not
success. A local controller has additional queued, starting, stopping and lost
states; keep these separate from wire states.

`message/stream` returns SSE records containing JSON-RPC responses. Parse across
arbitrary byte boundaries, including CRLF, multiline data and UTF-8 splits.
Validate status/artifact event IDs, honor append/lastChunk artifact semantics,
and distinguish stream-final from task-terminal: an input-required stream can
end while its task remains live. Cap each record, assembled artifact, queued
event count and total output. Never interpret artifact text as an instruction.

Follow-up user messages carry the task/context IDs. Only an operator or the
trusted orchestrator policy may steer a worker. The oxa adapter maps these to
RPC `steer` while running and `follow_up` when queued. It maps RPC completion
events to a terminal task only after inspecting the run outcome; a successful
RPC command response merely acknowledges the command. Deny permission dialogs
by default; route a request to the operator only within the parent's policy.

Cancellation sends `tasks/cancel`, then verifies terminal status. For owned
children also send RPC `abort`, wait a bounded grace period, terminate the
process group, and reap it. Retain final output already received. A terminal
completion racing cancellation wins once recorded; do not overwrite it. A remote
cancel acknowledgment is not proof that billing or execution stopped. If status
cannot be verified, record lost/stop-unconfirmed and retain its budget reserve.

After a stream disconnect, query `tasks/get` and resubscribe if supported.
Do not resend the original message automatically: JSON-RPC IDs are not
idempotency keys. Deduplicate known artifact/status updates without assuming
every server provides replay IDs. A restart never blindly replays a launch.

## Worker supervision and fan-out/fan-in

A run plan assigns each worker a role, explicit task text, endpoint/model,
policy envelope, output schema, deadline and budget. Validate the whole plan
before launching any worker. Enforce maximum parallelism and a global queue
bound. Spawn processes with argument arrays, never shell interpolation. Use
an allowlisted environment and isolated HOME/XDG/session directories; do not
inherit agent keys, unrelated MCP servers, extensions or project hooks.

Each local editing worker gets an isolated worktree. Its sandbox grants only
the planned paths. Read-only reviewers receive no write grants. Fan-in records
every worker's outcome, evidence and usage; it does not hide partial failures.
An explicit all-required or best-effort policy determines aggregate success.
Worktrees produce patch proposals; overlap triggers review, never automatic
conflict resolution or merge. Child recursion is disabled initially.

The adapter uses `oxa --mode rpc`, explicitly sets permission/offline/model
options, checks `get_state`, then sends its role and scoped task. Reuse
`oxa-rpc` only after verifying its launch, timeout and shutdown guarantees;
wrap missing supervision in the new crate rather than changing RPC internals.
All exit paths must close pipes, stop owned processes and join reader threads.

## Permission, offline and information boundaries

The effective worker envelope is the intersection of parent grants, operator
delegation and worker restrictions. Ask/workspace/plan modes are not a simple
privilege ranking: carry explicit read/write roots, network destinations,
allowed tools and approval requirements. Reject plans the adapter cannot
enforce. A card, returned message or model-selected command cannot modify the
envelope. Never pass `--yolo` as an orchestration shortcut. Existing sandbox
and deny-list checks remain mandatory; missing enforcement prevents launch.

Offline is inherited and cannot be cleared by workers. Only explicitly
registered loopback mock or verified local-inference endpoints are eligible.
**Loopback alone is insufficient:** an oxc service can forward to a cloud
provider. Require a trusted local-only route guarantee; if unavailable, refuse
that route offline. A remote card claiming to be local is not such a guarantee.
Use network isolation for child tools as well as host validation in transport.

Only the explicitly supplied task and approved artifacts cross a boundary.
Do not forward the parent transcript, environment, credential stores or ambient
filesystem contents. Secrets must be excluded by construction: initial remote
workers have no local filesystem tools, and tasks containing private material
are ineligible for remote delegation. Local workers receive isolated task
workspaces without secret files. Pattern redaction is defense in depth, not
proof that arbitrary source text is safe to export. If that isolation cannot
be established, refuse delegation. Treat remote results as untrusted data and
require local review before applying code or running suggested commands.

## oxc model mapping and budgets

Resolve each worker to an explicit route descriptor: agent endpoint, provider
class (mock/local/cloud), model ID, context limit, trusted network policy and
pricing version. Let oxc own provider credentials; never send them over A2A.
An oxa worker selects its authorized model through the existing RPC model
command. A failed selection prevents prompting; no silent fallback to a cloud
model or different price tier. Do not contact or reconfigure an installed oxc
service during development. Cloud execution needs a separate operator go.

Before launch reserve each worker's maximum spend from the run budget. Track
input, output, cached tokens, requests, elapsed time and monetary micro-units;
unknown usage stays unknown rather than becoming zero. Reserve output tokens
before requests and require trusted provider-side request/token caps for a
hard monetary ceiling. Post-hoc usage polling alone cannot guarantee that cap.
If a route cannot enforce the requested hard cap, reject it. Wall-clock and
output-byte caps still apply to free local/mock work. Cancel on the first cap
reached. Retries consume the same worker reservation and deadline; they do not
reset allowances. Keep uncertain remote spend reserved until reconciled.

## Transcript and failure handling

Write a private append-only JSONL journal with schema version, run/worker IDs,
monotonic sequence, event time, role, task reference, state transitions,
policy decisions, route class, usage and artifact hashes. Separate raw content
from the status journal and make its persistence opt-in. Never log tokens or
authentication headers. Bound journal size; on write failure stop launching
workers and cancel live work rather than continuing without an audit trail.
Demo logs contain synthetic content only.

| Failure | Controller action |
| --- | --- |
| Invalid card, version or permissions | Reject before scheduling |
| Auth failure | Fail closed; no credential prompt to a model |
| Child exit, broken pipe, malformed event | Mark failed, clean up, retain evidence |
| Stream timeout or disconnect | Query the known task; never duplicate launch |
| Deadline, spend or output cap | Cancel and supervise shutdown |
| Remote stop cannot be confirmed | Mark lost, preserve reservation, report uncertainty |
| Partial fan-in or overlapping patches | Report partial result and require review |
| Controller restart | Reconcile journal and known tasks before any new launch |

## Delivery and verification gates

1. This design-only draft PR: coordinator review of protocol, boundaries,
   route guarantees and manifest ownership. No existing-code hooks.
2. A2A crate draft PR: original wire types and bounded transports, loopback
   mock, unit and conformance tests. Cite every pinned spec example used as a
   fixture, retain required notices, and distinguish our supported profile
   from full protocol conformance. Cover malformed requests, JSON-RPC errors,
   direct Message replies, task identity, SSE chunk boundaries and cancellation.
3. Orchestrator draft PR: mock RPC processes proving parallel fan-out, steering,
   fan-in, deadlines, budgets, process reaping and transcript attribution.
   Include negative tests for permission widening, offline cloud routing,
   secret-bearing environments, cross-worker events and crash reconciliation.
   Coordinator review is required at this milestone too.
4. Opt-in CLI draft PR: bounded worker status and cancellation; help and normal
   startup remain unchanged without the feature. List every integration hook.
5. Model-free E2E draft PR and short demo log: one successful parallel run,
   one partial failure, one cap cancellation and one offline rejection. Use
   isolated HOME/XDG directories and loopback mocks only, with no paid calls.

Each draft must pass existing public hosted CI. Do not change Actions settings,
attach self-hosted runners, push main, merge, or force-push another lane.
No merge occurs without coordinator review plus the required T2/T3 sign-off.
The first cloud demo is a separate action requiring explicit operator approval
and tiny enforced budgets; mock E2E does not confer that approval.

## What we will not do

No automatic merge, arbitrary remote shell, autonomous permission escalation,
shared credential environment, public unauthenticated service, recursive agent
spawning, cloud calls in CI, model downloads, production service changes or
changes to another machine. No initial gRPC, push notifications, arbitrary file
URI fetches, third-party agent auto-discovery or assertion of full A2A support.

## Sources and licensing

Checked for this proposal:

- [A2A v0.3.0 specification](https://a2a-protocol.org/v0.3.0/specification/)
  and [tagged Apache-2.0 license](https://github.com/a2aproject/A2A/blob/v0.3.0/LICENSE).
  This document describes an independently implemented wire profile. Future
  copied schemas/examples must retain their applicable Apache notices/license;
  do not relabel upstream material as MIT.
- [T3 Code README](https://github.com/pingdotgg/t3code/blob/main/README.md)
  and [MIT license](https://github.com/pingdotgg/t3code/blob/main/LICENSE),
  copyright T3 Tools Inc. No T3 source code is copied here. If reused later,
  retain its copyright and license and update attribution notices.
- Existing oxa [RPC](../pages/rpc.md), [security](../pages/security.md) and
  [CLI](../pages/cli.md) documentation. Proposed guarantees above need tests;
  the current RPC interface alone does not establish them.
