# Chat Completions contracts (#334, slice 4)

Enable `upstream-openai-chat`. `openai_chat::request` validates message/tool shapes,
filters local-only parameters, selects only reviewed exact model IDs, disables
storage, caps output and requests streaming usage. Reasoning models remove sampling
parameters and map effort as Python does. No transport, key or budget access occurs.

`response` checks the native single-choice Chat shape and passes its JSON through
with the public model alias and Python's default tool type/index. `Stream::event` does the same for parsed chunks,
including partial tool arguments. Supply decoded SSE `[DONE]` via `done()` and
require `finish()` at EOF. A finish reason and `[DONE]` are both required. Only a
valid usage-only frame can become final usage; provisional per-choice counts are
ignored. Missing, partial, malformed, overflowing or inconsistent cache/reasoning
counts return no trusted usage, so the gateway can retain conservative accounting.
Input cache reads/writes stay separate and `uncached_input_tokens()` excludes both.

The gateway owns SSE framing, redaction and any shared OpenAI-compatible
normalization (e.g. nonstandard `reasoning` aliases or object-valued arguments).
This slice passes standard native Chat Completions fields intact. Tool names are
returned whole; no hold-back redaction runs inside this module.

Synthetic request fixtures use the Python translator with an in-process mock
upstream. Stream fixtures run Python's CloudStream and Meter. Rust compares
canonical JSON bytes and usage counts. Recording forbids sockets and clears the
environment under a private HOME; no API keys or real services are used.
`openai_contract.rs` is identical to the companion Responses slice; retain its
export for either feature when merging these independent main-based PRs.

```sh
python3 -I oxc-rs/tests/fixtures/record_openai_chat.py --check
cargo test --locked --manifest-path oxc-rs/Cargo.toml --features upstream-openai-chat
cargo check --locked --manifest-path oxc-rs/Cargo.toml --no-default-features
cargo clippy --locked --manifest-path oxc-rs/Cargo.toml --all-features --all-targets -- -D warnings
cargo fmt --manifest-path oxc-rs/Cargo.toml --all -- --check
```
