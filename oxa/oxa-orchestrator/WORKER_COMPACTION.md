# Budgeted worker compaction (mock-verified)

An operator profile can set `"auto_compaction": true` to let a long-running worker
compact its existing conversation. The setting defaults to false and is captured
with the fixed route/model and grants. Model tool arguments, agent cards, steering
and retargeting cannot enable it or change it.

This requires the feature-built native worker's [engine usage source](NATIVE_USAGE.md).
The supervisor refuses the opt-in before sending a prompt when that capability
is absent. Automatic agent retries stay disabled. The source checks the captured
model for summary/prefill calls as well as normal chat and counts every SDK
inference return without relying on presentation events.

For example, add this field to an otherwise approved local or gateway profile:

```json
{"auto_compaction": true}
```

Compaction uses the same native process, conversation, goal ID and budget. It
neither resets counters nor grants new turns, tokens, models or file access.
Summary and prefill usage belong to the current goal. A summary can exhaust the
observed token cap before the next goal turn. Failed/aborted summary usage stays
unknown; token-bounded work cannot continue with an unknown cumulative total.
Cancellation stops the owned process and its inference connection.

Runtime budget stops are published with their final state (`turn_stopped`), then
with the post-cleanup result. They do not first emit a misleading blocked-input
wake. Actual model-declared blocking still wakes the main session for clarification.

Worker wake previews now contain a compact child ledger, reply and review
availability rather than repeating the full goal text or proposal. This keeps a
long goal from crowding the result out of the bounded wake. The full goal and
review report remain available through `worker_lane status`.

## Evidence

```sh
cargo test -p oxa-orchestrator --features interactive-control --test shared_tui -- --compaction-only
```

The harness runs actual sandboxed RPC workers against owned loopback fake engines,
using a 14 KiB goal and a 50,000-token advertised context. It observes actual
compaction-kind inference and the summary marker in a later chat, verifies the
same session/model and unchanged goal budget across three turns, and reconciles
the final token ledger with every SDK request, including zero-output prefill.
The main session receives the worker reply even with the long goal text.

Additional cases set a cap just above the pre-summary usage, inject an
unrecoverable summary HTTP error, and cancel while a summary is in flight. They
check that the cap stops a third goal turn, failed usage remains null, a failed
summary is not silently retried, the canceled summary connection closes at the
fake endpoint, and worker PIDs are gone before a final result is published.
The error case uses an explicit nonrecoverable response; the SDK's existing
transport reconnect behavior is unchanged.

All implementation remains in `oxa-orchestrator`; no new Core/RPC/sandbox hooks
are added. This is worker compaction, not main-TUI goal compaction. Main-session
engine-level metering and compaction are still outstanding. No actual cloud
service, provider keys or paid calls were used. Provider billing/cancellation,
#64 continuation recovery, #47/#48/#71 settlement, upstream #60 integration, the
working-evening acceptance run and the required reviews remain unverified.
