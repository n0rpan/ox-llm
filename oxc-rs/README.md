# Native oxc (work in progress)

This independent crate builds the `oxc` binary. The controller uses Tokio and an
axum router; this crate forbids unsafe code and invokes no Python interpreter.
It is not a complete replacement yet and has no installation switch enabled.

The owner authorized starting Rust while the remaining conformance work and CI
proceed. Existing Python product code and reference goldens remain unchanged;
new tests still establish Python behavior before checking a ported path.

Build locally, keeping generated binaries outside the source checkout:

```sh
cargo build --locked --release --manifest-path oxc-rs/Cargo.toml --target-dir ../oxc-target
```

Local validation: 56 parser cases, five CLI ledger-file cases, seven empty-install
HTTP cases and API-key file rotation/fail-closed behavior pass in Rust. A native
CLI subset of 29 runtime cases passes against the Python daemon. HTTP gates are
16/17: valid gzip inference still fails because engine launch is not implemented.
The Python reference suite has 218 passing scenarios.

The initial port includes CLI grammar/help/error output, local ledger file reads,
cache controls, harness prepare/forget dispatch, load/unload dispatch, storage
listing/deletion dispatch, sharing status/on/off dispatch, and panel invocation.
The native `serve` entry point reads the existing config paths and implements
model metadata, Host/Origin/CORS/authentication gates, API-key file refresh and
fail-closed behavior, request IDs and the empty-install HTTP contracts.

HTTP/1 heads are bounded and parsed before axum dispatch so malformed content
lengths can receive the same JSON/authentication/error handling as Python. Bodies
are read lazily, so rejected requests do not require consuming untrusted content.
The CORS preflight's missing request ID is deliberately preserved bug-for-bug.

Engine launch/inference, complete status/control state, qualified Flash/classic
profiles, proof-gated long contexts, installers, update/rollback, secrets for
sharing/cloud, and the remaining commands are still pending. Flash/classic and
long-context profiles are not advertised as eligible in place of their missing
qualification adapters. Full Rust conformance currently fails and hardware A/B
readiness is not claimed.

Run the complete black-box suite to see the remaining differences:

```sh
OXC_COMMAND=/absolute/path/to/oxc \
OXC_DAEMON_COMMAND='/absolute/path/to/oxc serve' \
python3 -m unittest discover -s conformance -v
```

Local checks for the first complete slices:

```sh
OXC_COMMAND=/absolute/path/to/oxc python3 -m unittest discover -s conformance -p test_cli.py -v
OXC_COMMAND=/absolute/path/to/oxc python3 -m unittest discover -s conformance -p test_cli_files.py -v
OXC_DAEMON_COMMAND='/absolute/path/to/oxc serve' python3 -m unittest discover -s conformance -p test_http.py -v
cargo fmt --manifest-path oxc-rs/Cargo.toml --all -- --check
cargo clippy --locked --manifest-path oxc-rs/Cargo.toml --all-targets -- -D warnings
```

Keep the PR draft while iterating. Test locally; do not push repeatedly to trigger
public CI. Request a full CI/review deliberately, using one workflow dispatch
(or mark ready only when appropriate). Use one blocking `gh pr checks --watch`
when waiting is needed, never repeated polling.
