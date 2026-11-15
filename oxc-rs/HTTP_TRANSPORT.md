# Opt-in HTTP transport (#271, first slice)

Enable `http-transport` for `transport::serve_with_timeout`. The caller supplies an
async connection, an axum router, a per-stall timeout and whether to monitor EOF.
This library opens no listener, routes no provider, reads no configuration/key,
and starts no engine. Defaults remain empty; native CLI installation is unchanged.

The bounded HTTP/1 decoder retains duplicate/invalid Content-Length and original
header values for the application's auth/framing policy to reject in the proper
order. It dispatches one request at a time and leaves pipelined bytes untouched.
The router must reject invalid/conflicting request framing before consuming a body.
Host/Origin/auth/gzip decisions are a separate follow-up slice.

The writer handles fixed and chunked responses, SSE framing, peer disconnects,
and response finalizers. A stream error cannot emit a successful terminal chunk.
Fully delivered fixed-length responses can finalize after a client closes;
unfinished responses are canceled. Timeouts measure stalled operations rather
than total generation time. `serve` is an untimed convenience for controlled
connections; network callers should use `serve_with_timeout` with a finite limit.
HTTP/1.1 is the current controller protocol; no legacy serving path is exposed.

```sh
cargo test --locked --manifest-path oxc-rs/Cargo.toml --all-features
cargo clippy --locked --manifest-path oxc-rs/Cargo.toml --all-features --all-targets -- -D warnings
cargo check --locked --manifest-path oxc-rs/Cargo.toml --no-default-features
cargo check --locked --manifest-path oxc-rs/Cargo.toml --no-default-features --features http-transport
cargo fmt --manifest-path oxc-rs/Cargo.toml --all -- --check
```
