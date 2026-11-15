# Native oxc (work in progress)

This independent crate builds the `oxc` binary. The controller uses Tokio and an
axum router; this crate forbids unsafe code and invokes no Python interpreter.
It is not a complete replacement yet and has no installation switch enabled.

The owner authorized starting Rust while the remaining conformance work and CI
proceed. Existing Python product code and pre-existing goldens remain unchanged;
new tests still establish Python behavior before checking a ported path.

Cloud-only mode (no local engine) is the first complete runtime target. Its
gateway and installation contracts will follow #24 and the cloud-only stack
after they land on main. Hardware A/B waits for that suite to be green. See the
[reviewable merge plan](../conformance/MERGE_PLAN.md); draft
[#177](https://github.com/oxstack/ox-llm/pull/177) extracts only the opt-in parser
binary. Draft [#186](https://github.com/oxstack/ox-llm/pull/186) adds the independent
bounded control client on top. Draft
[#197](https://github.com/oxstack/ox-llm/pull/197) adds only the read-only ledger
CLI and its minimal helpers, with a separate file-reader conformance subset.
This integration branch stays draft #28.

Build locally, keeping generated binaries outside the source checkout:

```sh
cargo build --locked --release --manifest-path oxc-rs/Cargo.toml --target-dir ../oxc-target
```

Local validation: 140 parser cases, twelve CLI ledger-file/limit cases, seven empty-install
HTTP cases, all 18 HTTP gates, all 62 configured API cases, and API-key file
rotation/fail-closed behavior pass in Rust. All 18 lifecycle tests and 13 harness registry cases pass. Read-only
cache usage and storage inventory pass 14 additional reference cases. A native CLI set of 48 runtime cases passes against the Python daemon;
all 48 also pass with the native daemon.
Fifteen loopback CLI cases cover desktop dispatch, detached sessions and exec
failure reporting, inherited stdin/umask, service failures, model qualification,
selection and read-only planning. CLI replies are bounded to 64 KiB and control
timeouts follow the reference.
The request ledger passes 12 golden scenarios and eight additional checks;
Python/Rust/Python ledger round trips preserve the salt, hashes and row schema.
Twelve cache-budget cases, eight context-mode cases and all three file round-trip
contracts also pass. Eighteen scoped cache-clear cases pass. The Python reference
and native suites both pass 891 scenarios in 105 unittest methods.

Twenty-one engine-free HTTP cases need no fake model/engine or host-memory
namespace. They compare the real gates while another owner holds the inference
lease and verify no child/model, config mutation or lease replacement remains.
Regression cases establish classic-backend route/body parsing before the fix.
Nine gzip cases also check every member/checksum, zero padding, exact engine
work and the unchanged 16 MiB bound across all decompressed members. CLI
diagnostics preserve Python quoting and Unicode printability facts. All 280
additional isolated parser probes match the reference.
`conformance/run_suite.py cloud-foundation` selects these foundations plus CLI
contracts; `--native /absolute/path/to/oxc` sets every helper override explicitly.
Cloud-only validation remains an explicit pending-baseline error.

Native bootstrap/install passes 34 scenarios in 24 methods: pinned asset plans,
model-free native bundles, installed help and service startup, create-only file
transactions, hash/inode-bound journals and rollback, owner edits, canonical lease
preservation, refusals and Python/Rust journal interoperability. The inference
lease remains held through offline agent identity refresh and child cleanup.
Release size optimization keeps the executable inside the unchanged 8 MiB
payload bound. Bundles include no Python runtime; the two ExecStart lines become
`local-ai serve` and `local-ai studio`, with every other unit byte checked against
the original snapshots. Installation starts no service and enables no qualification.

Eighty-seven Studio cases cover the unchanged frontend, HTTP/1.0, request gates,
model metadata/settings, calibrated estimates, CPU identity proofs and Oxi routing
gates, private multipart uploads, descriptor-anchored file reads, MIME quirks and
malformed-input EOF. Further cases cover planner refusals, synthetic CPU jobs
and batches, anonymous prompt descriptors, validated CPU profiles/tuning,
activity and failure cleanup. A tiny fake image peer writes a deterministic
blank PNG without reading model contents; no real inference runs. Full
edit/upscale/refine, pixel operations and Oxi/NPU/image-slot adapters remain
pending, deferred behind cloud-only completion.

Thirty-eight receipt-tool cases cover the native scanner, exporter and scrubber:
reference rules/order, machine identifiers and explicit exemptions, Unicode line
limits, generated trees, argv/environment redaction, prompt/token hashes, numeric
and non-finite values, directory inputs, refusal before writing, helper help and
layout-preserving rewrites. The complete public serialization is scanned before
writing. Model-weight inputs discovered in directories are counted without being
read. Existing mandatory repository scanners and exemptions are unchanged.

Twenty-seven digest CLI cases cover numeric/enum aggregation, origin/text filtering,
stored days, trend and soak thresholds, private atomic writes, stale status, dates,
installed versions and Python/Rust/Python reads. Non-finite ledger fields remain
excluded from numeric calculations while preserving the reference record counts.
Digest commands never start a model. Generic staged update/rollback commands are also native.

Twenty-seven uninstall cases cover exact bootstrap journals/stage records, hash
checks and owner edits, dry-run/apply, positive service stops and restoration,
model retention and explicit deletion, symlinks/parent escape, private-home
refusal and partial failures. Repeated --record flags are preserved. Deletion
uses directory descriptors, repeats ownership/hash checks and never sweeps a
directory. All systemd interactions in these tests use a synthetic peer.

Fifteen staged-update scenarios (22 CLI snapshots) cover sealed/bound hashes,
exact restore, created/removed files, private state, receipts, positive controller
identity, agent identity refresh and refusal to overwrite owner edits during
recovery. The legacy guarded script supplies quoted JSON binding data and is
never executed. Generic manifest file transactions are implemented; generated
runtime materialization still refuses activation until its native adapter exists.

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
qualified Flash/classic and execution-override profiles, transfer/build flows,
generated runtime materialization, sharing/cloud secrets and further CLI edges are pending.
Automatic native engine-ledger capture passes twelve reference cases: profile
opt-in, a dedicated inherited pipe, numeric/enum filtering, bounded lines,
stdout isolation, private rotation, advisory storage failures and unload/reload
drain. Engine stdin remains `/dev/null`, and no unsafe Rust is used. Flash/classic
profiles remain ineligible while their qualification adapters are missing. Full
product parity remains incomplete; no installation switch or hardware readiness is claimed.

Run the complete black-box suite to see the remaining differences:

```sh
OXC_COMMAND=/absolute/path/to/oxc \
OXC_DAEMON_COMMAND='/absolute/path/to/oxc serve' \
OXC_BOOTSTRAP_COMMAND='/absolute/path/to/oxc __bootstrap' \
OXC_INSTALL_COMMAND='/absolute/path/to/oxc __install' \
OXC_EXPORT_RECEIPT_COMMAND='/absolute/path/to/oxc __export_receipt' \
OXC_SCRUB_RECEIPTS_COMMAND='/absolute/path/to/oxc __scrub_receipts' \
OXC_STUDIO_COMMAND='/absolute/path/to/oxc studio' \
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

The current [paired synthetic measurement](../conformance/benchmarks/rust-f8c482e-a874eac.json) verifies 100 measured requests plus warmup reached the same fixture with matching bodies and expected replies. Native idle/sampled-load RSS was 7,084/7,692 KiB and p50/p95 HTTP latency was 1.095/3.077 ms; the matching Python values were 19,696/20,504 KiB and 52.881/54.680 ms. Source, harness and executable identities are recorded. This measures controller overhead on the executor, with no hardware readiness claim.

Until v0.2.0 ships, keep all migration PRs draft. Test locally and keep following
main without review requests or full-CI dispatches. After release, the
orchestrator (Fable) and oxa handle reviews.

The CLI parser, control HTTP client and seven-command dispatcher are now separate
modules. Thirty-three new model-free control cases and a hosted-checkout namespace
regression establish peer calls, bounded errors, service/notification failures,
legacy JSON constants and per-stalled-read deadlines. The client slice includes
no engine, daemon, status projection, desktop launcher, installer or Python fallback.

Six model-free configuration contracts compare exact file bytes and identity.
Native config rewrites now use Python ASCII escaping, preserve unknown fields
and mode bits, atomically replace the target and leave symlink chains intact.
Both Unicode regressions were reproduced before the fix.

HTTP replies, error normalization, Host/Origin/CORS/auth checks and bounded gzip
decoding now live in a separate 428-line policy module. It depends on no engine,
model catalog, upstream proxy or sharing runtime. The daemon supplies its
sharing admission state through a generic context. Host checks still precede
configuration refresh, followed by Origin/auth checks in the original order.
This prepares a separate transport/policy slice without extracting the whole
daemon or porting an older cloud gateway.

Forty-nine engine-free HTTP policy cases use owned wire requests, synthetic key
files and a held foreign inference lease. Ten regressions failed before fixing:
the parser now retains significant trailing field whitespace, keeps empty bearer
precedence and Python header decoding, and applies Python's leading-slash
normalization. Navigation remains query-sensitive; null/CR/Unicode key-file
settings, health setup refusals and empty CORS request-header defaults match the
reference. All existing header/body/credential bounds are retained.
