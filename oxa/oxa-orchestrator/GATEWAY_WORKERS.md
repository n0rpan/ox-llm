# Operator-authorized gateway workers (mock-verified)

Version 2 of the private [worker profile file](WORKER_LANES.md) can authorize a
fixed model through an operator-selected literal-loopback inference gateway.
Version 1 and the public `register_verified_local` API continue to launch offline
workers. The model cannot authorize a gateway or change a route, role, model,
budget, or file grant through tool arguments, goal text, or an agent card.

An explicit gateway configuration has all four declarations: version 2,
`verified_local_only: false`, `allow_cloud_workers: true`, and a profile with
`route: "cloud_gateway"`. Missing or contradictory declarations are rejected.
Local profiles may coexist by using `route: "local"`, which asserts that their
route cannot forward to paid inference.

```json
{
  "version": 2,
  "verified_local_only": false,
  "allow_cloud_workers": true,
  "parallel": 1,
  "total_tokens": 20000,
  "total_output_bytes": 1048576,
  "profiles": [{
    "name": "cloud-reviewer",
    "role": "Review the assigned proposal and report concrete findings.",
    "route": "cloud_gateway",
    "engine": "127.0.0.1:8080",
    "model": "operator-approved-gateway-model",
    "budget": {"wall_ms": 10000, "tokens": 20000, "turns": 2},
    "turn_ms": 2000,
    "output_bytes": 1048576
  }]
}
```

These are configuration examples, not an authorization to perform a real cloud
run. No real gateway, installed service, provider key, or paid model was used in
verification. A real cloud experiment still requires Oscar's explicit go under
the orchestrator brief.

The file retains the same owner-only, single-link, canonical-path protections
and size limits as local profiles. Endpoints must remain literal loopback socket
addresses; DNS names and non-loopback engines are rejected. Provider credentials
belong to the gateway and are never passed to workers. There is no credential
field in this schema.

The parent CLI's hard offline policy (`--offline`, `OXA_OFFLINE`, or
`oxa.offline`) refuses gateway starts before source capture, reservation or
launch. The policy is checked again during runtime validation and immediately
before spawning. Local profiles always pass `--offline` to the child. Gateway
profiles omit that flag only after operator authorization and the parent policy
checks. Private homes, fixed tools and file grants, disabled extensions/MCP/web
search, clean environments, process ownership, and cancellation remain the same.
Online inference does not enable worker shell or network tools.

Socket status and result data identify the route; cloud-authorized workers have
`(cloud)` beside their model in the TUI footer. Model changes and retargeting
cannot change this captured authority. Lifetime token/output reservations remain
separate from the main session's ledger and are not refunded after cancellation,
unknown usage, forgetting, or rebinding.

Token limits are observed after usage reports; they are not prepaid USD caps and
can overshoot before usage is available. Time caps stop the owned runtime. The
gateway's monetary controls and accurate upstream accounting require independent
verification. Issues #47, #48 and #71 remain relevant; this change does not settle
provider cost or fix gateway accounting. Native workers still disable automatic
retries and compaction; long cloud sessions and continuation recovery (#64) need
further work before the working-evening acceptance run.

## Mock evidence

```sh
cargo test -p oxa-orchestrator --features interactive-control --test shared_tui -- --workers-only
```

The harness launches actual sandboxed native RPC workers against owned loopback
fake inference servers. It checks online gateway launches, unchanged offline
launches for local profiles, fixed roles/models, persistent context and budgets,
result wakes in the original session, and visible route status. Parent
cancellation closes the in-flight inference connections as observed at the fake
gateway and reaps the worker processes. An offline parent produces no worker
runtime or gateway generation. These tests verify client connection closure,
not actual provider cancellation or billing behavior.

For worker accounting across chat, summary and prefill requests, see
[NATIVE_USAGE.md](NATIVE_USAGE.md). This accounting step leaves automatic
compaction disabled and does not change gateway monetary settlement.
