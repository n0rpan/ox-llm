# Anthropic event translation (#334, slice 2)

Enable `upstream-anthropic-stream` to use `anthropic_stream::Stream`. It accepts
decoded Messages events and returns OpenAI chunk JSON values in order. The caller
supplies the timestamp. SSE decoding/encoding, redaction, transport and accounting
remain the gateway's responsibility; no network, key or clock access occurs here.

At EOF, call `finish()`. Missing message_stop, provider errors and malformed event
sequences fail with fixed errors; they never yield a trusted usage record. Only a
valid message_delta usage followed by message_stop produces final usage. Unlike
the Python wire, an initial-only usage estimate is never emitted as final usage.
Input, cache-read and cache-write tokens remain separate for correct pricing (#283).
Completed native blocks include thinking signatures and redacted data for the
caller's exact-prefix replay. Never log/persist opaque blocks as diagnostics.

Synthetic Python fixtures feed mock events directly into `_Stream`, with sockets
forbidden, a cleared environment and private HOME. Rust compares canonical JSON
bytes for chunk arrays, and exact native block/usage values. Multiple signature
deltas are concatenated so no opaque bytes are lost. All stop reasons, Unicode,
thinking, tools, partial JSON, invalid usage and usage opt-out are covered.

```sh
python3 -I oxc-rs/tests/fixtures/record_anthropic_streams.py --check
cargo test --locked --manifest-path oxc-rs/Cargo.toml --features upstream-anthropic-stream
cargo check --locked --manifest-path oxc-rs/Cargo.toml --no-default-features
cargo clippy --locked --manifest-path oxc-rs/Cargo.toml --all-features --all-targets -- -D warnings
cargo fmt --manifest-path oxc-rs/Cargo.toml --all -- --check
```
