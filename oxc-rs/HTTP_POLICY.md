# HTTP gate policy (#271, second slice)

Enable `http-policy` for `http_gate`. The caller supplies request context,
configuration and optional shared-listener admission state; this module creates
no listener, engine or upstream route. Host and canonical-target checks run
before configuration-dependent Origin/CORS/auth decisions. Current `OXC_API_KEY`
and configured absolute API-key files follow the Python contract, including
fail-closed unreadable/empty files and the control/health exemptions.

`Context::reply/error/stream` provide JSON/OpenAI errors, HEAD, request IDs,
Retry-After and CORS headers. Errors outside control routes use the same public
envelope, including root and non-v1 routes. `decompress_gzip` validates every
member/checksum and stops at 16 MiB decoded output. Body framing/JSON validation
and routing still belong to the composing handler.

The 49 synthetic wire cases exercise the real transport and gate with isolated
workers, cleared environments and private HOME/config/key files. Full Python
reference replies are freshly captured on current main while a foreign inference
lease is held. Admitted requests terminate at an explicit Rust test marker;
these tests claim gate parity, not runtime/model parity. Twenty-one additional
Python fixtures cover error envelopes. Gzip tests cover members, padding,
corruption, truncation and decompression bounds. No real API or credential is used.

```sh
python3 conformance/generate_http_policy_inputs.py --check
python3 -m unittest discover -s conformance -p test_http_policy_reference.py -v
cargo test --locked --manifest-path oxc-rs/Cargo.toml --all-features
cargo clippy --locked --manifest-path oxc-rs/Cargo.toml --all-features --all-targets -- -D warnings
cargo check --locked --manifest-path oxc-rs/Cargo.toml --no-default-features --features http-policy
cargo fmt --manifest-path oxc-rs/Cargo.toml --all -- --check
```
