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

Local validation: 65 parser cases, five CLI ledger-file cases, seven empty-install
HTTP cases, all 18 HTTP gates, all 62 configured API cases, and API-key file
rotation/fail-closed behavior pass in Rust. All 18 lifecycle tests and 13 harness registry cases pass. Read-only
cache usage and storage inventory pass 14 additional reference cases. A native CLI subset of 40 runtime cases passes against the Python daemon;
all 40 also pass with the native daemon.
Fifteen loopback CLI cases cover desktop dispatch, detached sessions and exec
failure reporting, inherited stdin/umask, service failures, model qualification,
selection and read-only planning. CLI replies are bounded to 64 KiB and control
timeouts follow the reference.
The request ledger passes 12 golden scenarios and eight additional checks;
Python/Rust/Python ledger round trips preserve the salt, hashes and row schema.
Twelve cache-budget cases, eight context-mode cases and all three file round-trip
contracts also pass. Eighteen scoped cache-clear cases pass. The Python reference
suite has 483 passing scenarios in 65
unittest methods.

The initial port includes CLI grammar/help/error output, local ledger file reads,
cache controls, harness prepare/forget dispatch, load/unload dispatch, storage
listing/deletion dispatch, sharing status/on/off dispatch, and panel invocation.
Chat/image desktop launch, model selection and read-only planning dispatch are
also native; plan qualification remains the controller's published decision.
Status, code and context-mode dispatch share a native filtered status projection.
Sixteen loopback cases cover invalid metadata, offline/stopping states, model
selection, sparse-file download receipts and active/duplicate/oversized/non-ASCII
trial queries. Status never loads a model or reads weight/prompt contents.
The daemon now publishes direct Oxe chooser facts, receipt-derived qualification,
prefix warmth for an exact model/engine pair, selected/loaded states, resident
bytes, idle/unload availability and shared harness facts. Twelve chooser goldens
and one additional status check cover metadata-only polling, receipt cache
invalidation, symlink refusal and busy/idle controls.
Fourteen sharing-control cases cover loopback status/off, bounded option parsing,
owner-only key metadata, private fingerprints, unchanged key files, active local
inference, validation and config symlink/permission preservation. Sixteen active-listener contracts now cover Tailscale discovery, authentication,
key revocation, route/header/payload restrictions, rates/body limits and keyed
remote ledger labels. Shared inference uses the loaded model without starting or
switching it, yields to local arrivals before and after streaming starts, and
keeps its slot through response and ledger finalization. Listener off/rebind
closes open connections, and per-client connection limits precede authentication.
Thirty-one network/TLS cases now cover LAN route/link discovery, private network
pins, missing gateway MACs, absent networks, automatic pause on a changed network
and explicit resume. TLS uses rustls with a TLS 1.2 minimum; invalid or mismatched
certificates refuse sharing. Verified HTTPS/TLS 1.2 and the reference omission of
its disconnect monitor for TLS sockets pass. A separate scenario observes real
60-second deadlines for stalled headers, bodies and TLS handshakes concurrently,
including slot release and no engine startup. Further network/TLS and admission
races, slow response readers and offline CLI fallback remain pending.
Seventeen host-side key cases cover show-once output, rotation, atomic private
files, cross-process locking, malformed/public/symlink refusal, path overrides,
Python/Rust/Python file reads and concurrent reveals by both implementations.
Raw keys appear only in the explicit CLI reveal, never HTTP status or logs.
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

The private request ledger uses an install salt and HMAC identities, records
JSON/SSE counts and timings, and compares bounded in-memory history/reply digests
without writing prompt or reply text. Cancellation and errors release the slot
after recording their outcome. Unload waits up to two seconds for a completed
request's bookkeeping, while generation remains busy. Failed advisory writes
do not fail inference. Recent-row reads, medians, retention and engine hint
headers follow the Python contracts, including existing file compatibility.

Read-only cache usage walks recognized files through directory descriptors and
reads bounded manifests. Storage inventory reads bounded identity headers, tracks
configured references and open/mapped paths, and returns no private paths.
Cache-budget changes persist atomically and evict expired/low-value sessions
under the inference lease. Pins, recent sessions, shared blocks and opaque models
retain the Python protections; unlink operations stay anchored to directory fds.
The lease/manager remain held if a control caller disconnects during mutation.
Scoped clear handles all/model/session/hint requests, preserves other owners'
shared files, atomically rewrites shared manifests with private permissions and
directory fsync, and removes recognized legacy Rust prefixes. Clear-all marks
registry entries cold while retaining their primers.
Classic PrefixStore clear preserves the reference constructor's private directory,
pending cleanup and pruning rules, then removes only recognized cache names and
syncs the directory. File operations use its opened directory descriptor.

The engine-owned cache adapter passes nine reference cases: capability-probed
launch flags, private overrides, disk off, authenticated/bounded control calls,
ownership recovery after a crash, and disk preload after explicit or asynchronous
model loads. Preload uses a recent ledger session or an in-memory hint when
ledger writes are disabled; stale engine generations cannot replace its status.
Controller eviction never substitutes for a failed engine-owned operation.
Seven legacy conversation-store cases verify argument preservation, separate/equal
forms, config/environment root overrides and private directory creation. Disk off
removes prefix/session persistence and its option flags; invalid roots refuse startup.

Fast/deep/max modes publish per-model limits and capabilities and preserve
unknown config fields, symlinks and permission bits. Max context requires the
same engine-hash-bound RSS proof and 27 GiB ceiling as Python. Cap changes unload
an idle model and refuse an active request; the synthetic engine verifies the
new context reaches its launch arguments. This is fixture validation only.

Complete status/control state and failure provenance, optional turn diagnostics, PrefixStore lookup/save,
qualified Flash/classic and execution-override profiles, installers,
update/rollback, sharing/cloud secrets and remaining CLI commands are pending.
Automatic native engine-ledger capture passes twelve reference cases: profile
opt-in, a dedicated inherited pipe, numeric/enum filtering, bounded lines,
stdout isolation, private rotation, advisory storage failures and unload/reload
drain. Engine stdin remains `/dev/null`, and no unsafe Rust is used. Flash/classic
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

Keep the PR draft while iterating and test locally. Draft checkpoint pushes run
formatting and mandatory scans. Request full PR CI/review deliberately by marking
ready, or use one workflow dispatch when needed. Use one blocking
`gh pr checks --watch` when waiting is needed, never repeated polling.
