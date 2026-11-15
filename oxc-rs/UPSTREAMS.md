# Pure upstream translators (#334)

The `upstream-anthropic-request` feature exposes `anthropic_request::translate`
and `translate_with_blocks`. No feature is enabled by default; these library
functions do not enable the native controller, read keys, send HTTP, or account
for spend. The gateway supplies the validated upstream model and catalog limit.
Errors contain fixed field names, never request contents.

`translate_with_blocks` lets the gateway restore native thinking blocks for the
exact assistant prefix. The caller must partition that lookup by upstream, model
and credential identity. Signed/redacted thinking blocks remain opaque and never
receive a cache breakpoint. System text, the final tool definition and the latest
non-thinking history block receive the Python reference's cache controls.

Synthetic fixtures are captured from the Python translator into an in-process
mock Messages upstream, with sockets forbidden and an empty environment/private
HOME. Rust compares canonical JSON bytes (object key order is unspecified; array
order, strings and native blocks are exact). The deliberate #334 corrections are
separate tests: tool-result arrays retain their text parts and empty tool results
become `[empty tool result]`, never an empty text block.

```sh
python3 -I oxc-rs/tests/fixtures/record_anthropic_requests.py --check
cargo check --locked --manifest-path oxc-rs/Cargo.toml --no-default-features
cargo test --locked --manifest-path oxc-rs/Cargo.toml --features upstream-anthropic-request
cargo clippy --locked --manifest-path oxc-rs/Cargo.toml --all-features --all-targets -- -D warnings
cargo fmt --manifest-path oxc-rs/Cargo.toml --all -- --check
```
