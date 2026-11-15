# Experimental native controller: HTTP transport and policy library

This slice exposes the existing HTTP transport and policy as an opt-in Rust
library: 972 lines in two modules, plus the library entry point. It includes
bounded HTTP/1 framing, stalled-I/O/disconnect handling, reply/error formatting,
Host/Origin/CORS/API-key gates and gzip decoding. It imports no engine, model
catalog, daemon, upstream adapter or sharing runtime. A generic context lets a
later runtime supply its own sharing admission state.

The existing CLI remains unchanged. There is no daemon command or installation
switch in this slice. Without `native-controller` the library is empty and no
native executable is built. All added dependencies are optional.

Forty-nine frozen Python wire inputs exercise the actual Rust transport and gate
functions. Refusals/preflight replies match the captured source responses. When
source dispatch returns setup/unknown-local-route errors after passing its gates,
the library test stops at an explicit admitted marker. This proves the policy
boundary, not a complete controller or cloud-only mode. Every test worker has a
fresh HOME and cleared environment; only synthetic keys and owned loopback peers
are used. A separate lightweight reference runner reproduces all 49 complete
source replies while holding a foreign inference lease. Two transport tests
check cancellation versus finalization after a client disconnects.

```sh
python3 conformance/generate_http_policy_inputs.py
python3 conformance/test_http_policy_reference.py -v
cargo test --locked --manifest-path oxc-rs/Cargo.toml --features native-controller
```

The shared input module regenerates the Rust fixture; output goldens are unchanged
copies of the integration captures. Both product modules differ from the tested
integration code only in library visibility. Tier T3; all original CI/security
controls remain, and the hosted x86_64/arm64 gate remains skipped on drafts.

The inherited CLI subset has 140 parser cases, 33 control cases, twelve ledger
cases and a hosted-checkout regression. Thirteen remaining command families
still refuse execution. Run it against both Python and the opt-in native build:

```sh
cargo build --locked --release --manifest-path oxc-rs/Cargo.toml --features native-controller
python3 -m unittest discover -s conformance -p 'test_cli*.py' -v
OXC_COMMAND=/absolute/path/to/oxc-native python3 -m unittest discover -s conformance -p 'test_cli*.py' -v
python3 conformance/check_native_scaffold.py /absolute/path/to/oxc-native
```

Stacked on ledger #197, controls #186 and parser #177. #28 remains the integration
branch. The cloud-only stack is now on main in #180; its complete runtime
contracts are the next priority, followed by the gateway port. This library
extracts only the front policy and transport.

Keep all migration PRs draft until v0.2.0 ships. Tests run locally; no review or
full-CI dispatch is requested. Post-release reviews use the orchestrator (Fable)
and oxa.
