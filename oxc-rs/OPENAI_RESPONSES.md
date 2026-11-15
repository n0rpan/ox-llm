# OpenAI Responses translation (#334, slice 3)

Enable `upstream-openai-responses`. `openai_contract::validate` checks message,
tool and tool-choice shapes before any continuation lookup. `openai_responses`
exposes `request`, `response`, parsed-event `Stream`, and `HistoryStore`. All are
pure library code: no HTTP, keys, clock access, budget or installation changes.

The gateway supplies a non-secret scope partitioned by upstream endpoint and
credential identity, plus monotonic seconds. Model and canonical conversation
prefix are hashed into each store key. Text arrays and equivalent JSON arguments
share identity; changed history, image content, scope or model cannot share state.
Defaults match Python: 128 entries, 8 MiB native output, 30-minute expiry, LRU
eviction. No opaque output is logged or persisted. Caller synchronization is
explicit through `&mut HistoryStore`.

Feed decoded provider events to `Stream::event`, then require `finish()` at EOF.
It returns the terminal provider response; only then emit SSE `[DONE]`. Redact the
normalized assistant as the client will see it and call `remember_completed` with
that assistant and terminal response. This retains native encrypted reasoning
alongside function calls for the next exact-prefix request, and rejects failed or
malformed terminal responses. Incomplete responses are never remembered. The
low-level `remember` operation is for already validated native output.

Evicted closed tool steps replay only their visible calls and results when a later
user/assistant step exists (#251). Missing state for the latest open continuation
returns 409 `cloud_continuation_unavailable`. No other prefix's reasoning is used.
Final usage requires nonnegative integer input/output counts; malformed, missing
or overflowed counts never yield a trusted usage object.

Synthetic fixtures capture Python request kwargs at the mock upstream seam and
parse Python stream wire output into chunk values. Canonical JSON bytes preserve
string/array order exactly; sockets are forbidden with empty environment/private
HOME. Rust also tests a 140-step loop with a two-entry store, expiry, byte bounds,
scope/model/prefix isolation, and failed/truncated streams.

```sh
python3 -I oxc-rs/tests/fixtures/record_openai_responses.py --check
cargo test --locked --manifest-path oxc-rs/Cargo.toml --features upstream-openai-responses
cargo check --locked --manifest-path oxc-rs/Cargo.toml --no-default-features
cargo clippy --locked --manifest-path oxc-rs/Cargo.toml --all-features --all-targets -- -D warnings
cargo fmt --manifest-path oxc-rs/Cargo.toml --all -- --check
```
