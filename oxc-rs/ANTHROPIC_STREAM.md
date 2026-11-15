# Anthropic event translation (#334, slice 2)

Enable `upstream-anthropic-stream` to use `anthropic_stream::Stream`. It accepts
decoded Messages events and returns OpenAI chunk JSON values in order. The caller
uses the shared implementation in `oxa/oxc-gateway`; the native module re-exports
it. Fixtures and behavior tests live beside that shared implementation. The caller
supplies the timestamp. SSE framing, transport and accounting remain the gateway's
responsibility; no network, key or clock access occurs here. Production callers use
`Stream::with_redactor` with the gateway's `Redactor` adapter. Its feed/finish hooks
keep independent hold-back state for text, thinking and each tool's arguments;
atomic tool names and IDs use `whole`, never a hold-back buffer. Held tails flush
before the finish chunk. Completed.assistant contains the same safe text as the
chunks, while native signed blocks stay unmodified for private replay. `Stream::new`
uses Identity and is only for callers without sensitive data and unredacted tests.

At EOF, call `finish()`. Missing message_stop, provider errors and malformed event
sequences fail with fixed errors; they never yield a trusted usage record. Only a
valid output_tokens count in message_delta itself followed by message_stop produces
final usage; empty/cache-only terminal usage never reuses an initial estimate. Unlike
the Python wire, an initial-only usage estimate is never emitted as final usage.
Input, cache-read and cache-write tokens remain separate for correct pricing (#283).
Completed native blocks include thinking signatures and redacted data for the
caller's exact-prefix replay. Never log/persist opaque blocks as diagnostics.

Synthetic Python fixtures feed mock events directly into `_Stream`, with sockets
forbidden, a cleared environment and private HOME. Rust compares canonical JSON
bytes for chunk arrays, and exact native block/usage values. All stop reasons, Unicode,
thinking, tools, partial JSON, invalid usage and usage opt-out are covered. Repeated
signature deltas replace the prior signature exactly as Python does. Synthetic
marker splits and held tails are compared against Python's stateful redactor too.

```sh
python3 -I oxa/oxc-gateway/tests/fixtures/record_anthropic_streams.py --check
cargo test --locked --manifest-path oxa/oxc-gateway/Cargo.toml --features upstream-anthropic-stream
cargo check --locked --manifest-path oxc-rs/Cargo.toml --features upstream-anthropic-stream
cargo check --locked --manifest-path oxc-rs/Cargo.toml --no-default-features
cargo clippy --locked --manifest-path oxc-rs/Cargo.toml --all-features --all-targets -- -D warnings
cargo fmt --manifest-path oxc-rs/Cargo.toml --all -- --check
```
