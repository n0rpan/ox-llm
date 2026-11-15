# Owned worker inference accounting

The feature-built CLI can meter every `Engine.chat` call in an owned native
worker, independently of whether the core emits a chat reply, a compaction event,
or no presentation event at all. This includes summary requests and zero-output
prefill. The main TUI goal ledger is separate. Worker compaction is an explicit
operator opt-in described in [WORKER_COMPACTION.md](WORKER_COMPACTION.md).

The existing optional dependency-augmentation hook installs a forwarding engine
decorator only when the private child environment requests it. Ordinary CLI/RPC
sessions emit no new records. The decorator forwards model information, exact
prompt counting, token counting and hint cleanup as well as inference. It checks
the captured worker model before delegating inference. No provider keys, prompts,
replies, model names, or session hints are included in the accounting records.

Owned workers request the source with `OXA_OWNED_WORKER_USAGE=1` in their clean
private environment. This is an internal supervisor protocol, not a public CLI
option. The source writes complete JSON lines to the same stdout pipe as RPC:

```json
{"type":"oxa_owned_inference","phase":"ready","version":1}
{"type":"oxa_owned_inference","phase":"start","id":1}
{"type":"oxa_owned_inference","phase":"end","id":1,"tokens":123}
```

IDs are monotonic per process. `tokens` is the returned SDK prompt plus completion
count, including cached input once. A failed call ends with `tokens: null`.
Accounting must be writable before the provider is called; failure to write an
end record also fails the request. The supervisor independently enforces bounded
records, at most eight active IDs, exact pairing, no duplicates, checked sums,
and unknown usage after malformed, failed or unfinished accounting.

After readiness, the supervisor uses this source rather than adding chat-message
usage a second time. Streaming usage remains provisional until the corresponding
call completes. It waits for active source calls to drain before treating a core
settlement as completion. Unknown usage never becomes known zero. A time-only
goal can still resume after interruption with unknown cumulative tokens; a
budgeted fan-out cannot claim verified success with unknown usage. Known usage
continues to enforce the observed token cap even when other usage is missing.

A legacy worker without the source retains chat-only accounting. Compaction from
such a worker is refused, rather than accepted without accounting. The session
worker tool captures the currently running feature-built CLI, and its mock
integration tests require `usageMeter: true` in the audit. The public RPC crate
and its normal wire format are unchanged.

## Verification

```sh
cargo test -p oxa-orchestrator --features interactive-control
```

Engine-decorator tests exercise chat, compaction-kind and prefill calls against
the in-process fake, then a failed summary. They compare the exact sum and verify
that accounting records contain no source/task text. Further tests verify
unavailable-accounting and model-mismatch refusal before inference, duplicate
rejection and unknown unfinished requests.

The real native-process/shared-TUI cases require source readiness in the worker
audit and compare the final goal counters with all calls observed by the owned
loopback inference endpoints. They also exercise context retention, blocked
retarget/resume, steering, cancellation, gateway policy and process cleanup.
The native harness enables the CLI's existing `orchestrator` feature so it tests
the same augmentation hook as the actual feature-built binary.

No new hooks in existing crates or public RPC changes are added. No production
service, provider key, real gateway or paid model is used. Provider-reported
monetary costs and gateway settlement remain separate; this source counts the
SDK's reported inference tokens. The main TUI has a separate
[engine source and compaction opt-in](MAIN_USAGE.md). Long cloud runs and issues
#47/#48/#64/#71 remain open.
