# Native oxc (work in progress)

This independent crate builds the `oxc` binary. The controller uses Tokio and an
axum router; this crate forbids unsafe code and invokes no Python interpreter.
It is not a complete replacement yet and has no installation switch enabled.

The owner authorized starting Rust while the remaining conformance work and CI
proceed. Existing Python product code and pre-existing goldens remain unchanged;
new tests still establish Python behavior before checking a ported path.

Build locally, keeping generated binaries outside the source checkout:

```sh
cargo build --locked --release --manifest-path oxc-rs/Cargo.toml --target-dir ../oxc-target
```

Local validation: 56 parser cases, five CLI ledger-file cases, seven empty-install
HTTP cases, all 18 HTTP gates, all 62 configured API cases, and API-key file
rotation/fail-closed behavior pass in Rust. All 18 lifecycle tests and 13 harness registry cases pass. Read-only
cache usage and storage inventory pass 14 additional reference cases. A native CLI subset of 29 runtime cases passes against the Python daemon.
The Python reference suite has 265 passing scenarios in 41 unittest methods.

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

The non-Flash engine manager owns lazy startup, SHA verification, the exclusive
lease, per-launch private bearer files, the GUARD-2b memory monitor, bounded
private diagnostic logs, unload/reload and idle reaping. A safe exec helper
creates the engine's session and private umask without unsafe pre-exec hooks.
Admission uses a single FIFO slot with bounded queue wait. TCP disconnects cancel
upstream work and release the slot; streaming replies are translated incrementally.

Chat, text completions and the stateless Responses API support JSON and SSE,
tools, reasoning options, usage details and normalized errors. Additional
Responses and guard/log contracts were established against Python before porting.
The log redactor is also tested at every pipe chunk boundary. The registry
preserves opt-in learning, metadata-only unknown candidates, private primers,
approval/ignore/label/prepare controls and compatibility counters. Python/Rust
round trips preserve registry state; completion finalizers survive a client
closing after it receives the full response.

Read-only cache usage walks recognized files through directory descriptors and
reads bounded manifests. Storage inventory reads bounded identity headers, tracks
configured references and open/mapped paths, and returns no private paths.

Complete status/control state, request-ledger writes, disk-cache launch adapters,
qualified Flash/classic profiles, proof-gated long contexts, installers,
update/rollback, sharing/cloud secrets and remaining CLI commands are pending.
State-cache-capable binaries and automatic native engine-ledger capture explicitly
refuse launch until their adapters are implemented. Flash/classic and long-context
profiles remain ineligible while their qualification adapters are missing. Full
Rust conformance fails; no installation switch or hardware readiness is claimed.

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
