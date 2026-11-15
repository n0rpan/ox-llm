# oxc drop-in conformance

Reference: main `194487ef05167c6932a63103f1d5ff0044b13e33`.
The current suite covers 891 scenarios in 105 unittest methods. This remains a
partial contract suite, not a complete parity claim. The owner authorized starting Rust in parallel with the remaining conformance
work. The initial native crate is in `oxc-rs/`; full Rust parity is not yet claimed.

Cloud-only mode is the first complete runtime target. See the
[merge plan](MERGE_PLAN.md) for separate review slices; the first parser slice is
draft [#177](https://github.com/oxstack/ox-llm/pull/177). The integration branch
remains in draft #28. The next independent control-client slice is draft
[#186](https://github.com/oxstack/ox-llm/pull/186), stacked on #177. The read-only
ledger CLI is draft [#197](https://github.com/oxstack/ox-llm/pull/197), stacked on
#186, with 12 file/limit cases and only its minimal reader helpers. Capture cloud gateway contracts after #24 lands on main,
then port them and the cloud-only stack. Hardware A/B waits until cloud-only
conformance is green.

The review runner audits that every captured method belongs to one slice and
sets all seven native command overrides. For example:

```sh
python3 conformance/run_suite.py cloud-foundation --python
python3 conformance/run_suite.py cloud-foundation --native /absolute/path/to/oxc
python3 conformance/run_suite.py all --list
```

The 21 engine-free HTTP cases install no engine/model and use no hardware
fixtures or bubblewrap. They hold a foreign inference lease, compare the real
HTTP replies and verify no child, local model or lease/config mutation remains.
Three new failures before the fix established the classic-backend unknown-route
and body-parsing behavior. Together with CLI cases they form `cloud-foundation`;
this is not cloud-only parity. The runner refuses `cloud-only`, `cloud-gateway`
and `cloud-installer` explicitly until their main reference contracts exist.

Requires Linux, Python 3.10+, OpenSSL, util-linux `setpriv`, and bubblewrap with unprivileged
user, network and cgroup namespaces. The lab CI job already installs bubblewrap. Run from the repository root:

```sh
python3 -m unittest discover -s conformance -v
```

The suite invokes the shipped Python `runtime/local-ai` entry script under the
public executable name `oxc`, and the unmodified controller daemon as a separate
process. Each invocation gets a temporary HOME and explicit XDG directories; its
environment is constructed from an allowlist and inherits no API credentials or
proxy settings. HTTP connections target loopback only. Parser cases stop before
command execution, so they do not contact desktop applications or systemd.
Runtime CLI cases use a fresh network namespace each, giving the CLI its
hardcoded port 8092 without reaching another service. Systemd and desktop
executables are synthetic peers on PATH; their exact command/argument lists are
part of the goldens. Shared-listener cases add synthetic addresses to isolated
loopback interfaces using setup-only network capabilities in a new user namespace.
They drop all capabilities with `setpriv` before starting either implementation;
no host network interfaces or routes are changed. TLS certificates and keys are
generated inside temporary homes and never committed. The I/O limit scenario
observes the unchanged 60-second limits concurrently for three stalled clients. The fixed HOME path in that namespace is synthetic, making
storage IDs reproducible across test runs.

Configured HTTP tests launch the unchanged daemon and a hash-pinned synthetic
engine executable through its real lifecycle. A private bubblewrap mount/cgroup
namespace supplies deterministic host memory, PSI and cgroup counters, including
the installed service's `memory.swap.max=0`. Changing those fixture files tests
the real guard's refusal and recovery, including runtime faults and sustained PSI.
A private /tmp supplies an owned ancestor for the existing diagnostic log path
checks; the executor's host root UID is unmapped inside bubblewrap. Diagnostic
contracts verify private permissions, visible control characters and refusal of
symlink/hard-link targets. No runner cgroup settings are changed.
The synthetic `/proc` view delegates other metadata to live procfs and exposes
engine PIDs before readiness. The daemon receives SIGTERM directly for graceful
shutdown, rather than terminating the bubblewrap supervisor. Daemon fixtures set
the installed service's private umask and restore the caller's setting on exit;
permission checks are independent of the runner's shell umask.

Configured wire goldens contain both client replies and the exact inference
requests received by the fake engine. SSE event names and frame order are
preserved. Random completion/Responses IDs and timestamps are validated before
normalization; repeated IDs within a response retain the same marker. Warning,
ignored-parameter and CORS headers are compared when present.

To test another implementation, set shell command strings (parsed with shlex,
never executed through a shell):

```sh
OXC_COMMAND='/absolute/path/to/oxc' \
OXC_DAEMON_COMMAND='/absolute/path/to/oxc serve' \
python3 -m unittest discover -s conformance -v
```

Those two overrides exercise the controller and runtime CLI. Bootstrap/install,
Studio and receipt tools have separate command overrides; leaving them unset runs the
Python reference for those components. To check the native helper
entry points explicitly, also set:

```sh
OXC_COMMAND='/absolute/path/to/oxc' \
OXC_DAEMON_COMMAND='/absolute/path/to/oxc serve' \
OXC_BOOTSTRAP_COMMAND='/absolute/path/to/oxc __bootstrap' \
OXC_INSTALL_COMMAND='/absolute/path/to/oxc __install' \
OXC_EXPORT_RECEIPT_COMMAND='/absolute/path/to/oxc __export_receipt' \
OXC_SCRUB_RECEIPTS_COMMAND='/absolute/path/to/oxc __scrub_receipts' \
OXC_STUDIO_COMMAND='/absolute/path/to/oxc studio' \
python3 -m unittest discover -s conformance -v
```

The captured native helper cases pass with those explicit overrides. This is a
partial contract suite; complete runtime parity and hardware readiness remain unfinished.
Bootstrap's native bundle changes only the two service ExecStart lines to
`local-ai serve` and `local-ai studio`; separate unit snapshots assert every
other byte remains unchanged. Installation stays unqualified and starts no services.

The daemon command must use the existing `LOCAL_LLM_PORT`, `LOCAL_LLM_CONFIG`,
`LOCAL_AI_LEASE` and XDG settings. `serve` above is illustrative; its final public
invocation remains to be established from the installation contract.

Goldens are reviewable JSON containing exact exit codes, stdout/stderr, HTTP
status, content type and body. HTTP request IDs must match `req_` plus 32 lowercase
hex digits; only that validated random value is replaced by a marker. Python
currently generates an ID even when the client supplies one. Missing config
currently causes model discovery to return 503, including unknown model IDs.
These behaviors are preserved without changing Python. `n > 1` is refused
with 400. The explicit bug-for-bug CORS preflight case records Python's missing
`x-request-id` on its 204 response.

Intentional reference refresh, after reviewing changes on main:

```sh
python3 conformance/test_cli.py --record-python
python3 conformance/test_cli_files.py --record-python
python3 conformance/test_cli_edges.py --record-python
python3 conformance/test_cli_limits.py --record-python
python3 conformance/test_http.py --record-python
python3 conformance/test_http_policy.py --record-python
python3 conformance/test_config_files.py --record-python
python3 conformance/test_api.py --record-python
python3 conformance/test_http_gates.py --record-python
python3 conformance/test_gzip.py --record-python
python3 conformance/test_engine_free.py --record-python
python3 conformance/test_state_cache.py --record-python
python3 conformance/test_cache_budget.py --record-python
python3 conformance/test_cache_clear.py --record-python
python3 conformance/test_context_modes.py --record-python
python3 conformance/test_engine_cache.py --record-python
python3 conformance/test_engine_ledger.py --record-python
python3 conformance/test_conversation_stores.py --record-python
python3 conformance/test_storage.py --record-python
python3 conformance/test_harnesses.py --record-python
python3 conformance/test_ledger.py --record-python
python3 conformance/test_cli_runtime.py --record-python
python3 conformance/test_cli_control.py --record-python
python3 conformance/test_cli_actions.py --record-python
python3 conformance/test_cli_status.py --record-python
python3 conformance/test_cli_digest.py --record-python
python3 conformance/test_catalog.py --record-python
python3 conformance/test_share_control.py --record-python
python3 conformance/test_share_keys.py --record-python
python3 conformance/test_share_live.py --record-python
python3 conformance/test_share_network.py --record-python
python3 conformance/test_share_io.py --record-python
python3 conformance/test_scans.py --record-python
python3 conformance/test_studio.py --record-python
python3 conformance/test_update.py --record-python
python3 conformance/test_uninstall.py --record-python
```

Recording rejects alternate implementation command overrides. Review the diff;
do not refresh goldens merely because an alternate implementation fails them.

| Area | Python | Rust | Coverage still required |
| --- | --- | --- | --- |
| CLI parsing | PASS, 65 original + 75 additional cases | PASS: help/ambiguity precedence, Unicode integers, bounds, quoted/control-character diagnostics | More option combinations and non-UTF-8 argv |
| CLI ledger files | PASS, 5 original + 7 limit cases | PASS: private/bounded reads, Unicode integers and large-limit clamps | Additional historical ledger labels |
| CLI controls | PASS, 33 model-free cases + one namespace regression | PASS: exact peer/system calls, failures, bounded replies/reasons, legacy JSON constants and stalled-read deadlines | More metadata/control combinations |
| CLI runtime | PASS, 48 cases | PASS: all 48 cases against both Python and native daemons | More combined flags and filesystem failures |
| CLI desktop/model dispatch | PASS, 15 cases | PASS: detached session and exec handshake, inherited stdin/umask, service failures, bounded model qualification/errors, selection and read-only planning | More malformed replies and command races |
| CLI status/code/mode | PASS, 16 cases | PASS: filtered metadata, offline/stopping states, selected-model dispatch, Flash download phases and bounded trial queries | More malformed metadata and clock races |
| CLI health digests | PASS, 27 cases | PASS: numeric/enum reduction, origin/text filtering, stored days, trend/soak thresholds, private atomic writes, stale status, date validation, installed versions and Python/Rust/Python reads | More malformed numeric/file inputs and interruption races |
| Chooser/status metadata | PASS, 12 goldens and one additional check | PASS: direct Oxe chooser facts, selected/loaded states, prefix identities, receipt qualification/invalidation, resident/idle/unload and shared harness facts | Detailed failure provenance, Flash/classic adapters and more status fields |
| Sharing controls | PASS, 14 cases | PASS: loopback status/off, options, key metadata/privacy, busy inference, validation, config symlinks/permissions and absent setup | Further network/TLS races and offline CLI fallback |
| Sharing keys | PASS, 17 cases | PASS: show once, rotation, private atomic files, real cross-process locks, malformed/public/symlink refusal, overrides and Python/Rust file/lock interoperability | More filesystem failures |
| Shared listeners | PASS, 16 cases | PASS: Tailscale listeners, authentication, key revocation, filtered routes/headers/payloads, keyed remote ledger, rates/body limits, loaded-model admission, local preemption before/after headers, connection limits and off/rebind/drop | Further network/TLS and admission/finalization races |
| Sharing network/TLS | PASS, 31 cases | PASS: LAN route/link selection, persisted network pins, missing MAC/absent network, automatic pause/resume, malformed discovery/bind failures, TLS certificates/mismatch, verified HTTPS/TLS 1.2 and the reference TLS-disconnect behavior | More IPv6, certificate refresh and network/config races |
| Shared I/O limits | PASS, 1 scenario | PASS: concurrent real 60-second stalled-header/body/handshake limits, connection close and slot release without engine startup | Slow response readers and more TLS cancellation races |
| HTTP empty install | PASS, 7 cases | PASS | Additional setup failures |
| Configured HTTP | PASS, 62 cases | PASS: chat, completions and Responses JSON/SSE | More option combinations and engine error paths |
| Cache usage | PASS, 9 cases | PASS | More malformed/advisory I/O cases |
| Cache budgets | PASS, 12 cases | PASS: persistence, value/TTL eviction, pins/recent sessions, shared blocks, orphan echo, opaque models, lease refusal and symlink safety | More mutation races |
| Cache clear | PASS, 18 cases | PASS: all/model/session/hint scopes, shared/anonymous owners, opaque manifests, private durable rewrites, busy/lease refusal, classic/Rust prefixes and retained primers | More mutation races |
| Engine-owned cache | PASS, 9 cases | PASS: launch flags, disk off, private override, authenticated usage/budget/clear, bounded replies, explicit/async preload, saved/in-memory hints and crashed-owner recovery | More preload states/races |
| Numeric engine ledger | PASS, 12 cases | PASS: dedicated pipe, profile opt-in, numeric/enum filtering, bounded lines, stdout isolation, private rotation, advisory failures and unload/reload drain | More malformed numeric and stop races |
| Legacy conversation stores | PASS, 7 cases | PASS: separate/equal flag forms, private config/environment overrides, disk-off stripping and invalid-root refusal | More combined forms and engine-side persistence |
| Context modes | PASS, 8 cases | PASS: limits/capabilities, fast/deep/max, RSS-proof gates, active/cap-changing controls and private config writes | Execution override adapters and more malformed profiles |
| Storage inventory | PASS, 5 cases | PASS | Deletion, races and additional identity variants |
| HTTP gates | PASS, 18 cases | PASS | More malformed framing variants |
| Engine-free HTTP foundation | PASS, 21 cases | PASS: held foreign lease, no local engine/model/child or config mutation, backend-specific route/body parsing | Actual cloud-only catalog/routing/install after the upstream stack lands |
| Gzip request bodies | PASS, 9 cases | PASS: concatenated/empty members, padding, per-member integrity and the unchanged combined 16 MiB bound; exact fake-engine requests | More malformed headers and encoding combinations |
| Lifecycle | PASS, 18 tests | PASS | Pressure keep-warm, image handoff, crash/reaper paths, CPU pinning |
| Harness registry | PASS, 13 cases | PASS: learning/privacy, controls, prepare, streaming, limits and Python/Rust file reads | More malformed/advisory-I/O cases |
| Request ledger | PASS, 12 goldens and 8 additional checks | PASS: private HMACs, JSON/SSE records, history/reply tracking, errors/cancel, retention, limits, engine hints, advisory I/O and unload finalization | Optional turn diagnostics, queue/replay modes and more malformed backend timings |
| Model-free config files | PASS, 6 cases | PASS: exact JSON bytes, Unicode/control/astral escaping, unknown fields, mode bits, atomic replacement and stable symlink chains | Additional numeric encodings and filesystem races |
| Engine-free HTTP policy | PASS, 49 wire cases | PASS: header ordering/duplicates/whitespace, navigation queries, health/key exemptions, nullable/Unicode/CR key files and CORS defaults; ten regressions reproduced before fixing | Further malformed field syntax, key encodings and persistent-connection cases |
| Files | PASS, 3 tests | PASS: config and ledger Python/Rust/Python, API-key refresh/fail-closed | Additional state stores and sharing secrets |
| Cloud gateway / cloud-only | Main baseline pending #24 and the cloud-only stack | Deferred until reference contracts land; first complete runtime priority | All three providers, loopback mocks, continuation, reservations/settlement, budgets, ledger, redaction, off/offline, engine-free installation |
| Cross-implementation file reads | Harness/ledger ready; Python/Python PASS | Harness and ledger Python/Rust/Python PASS | Other state stores |
| Bootstrap/install | PASS, 34 scenarios / 24 methods | PASS: pinned plans, native bundle, installed help and both services, unchanged 8 MiB bound, create-only transactions, journals, leases, owner edits, refusals, agent refresh and Python/Rust rollback | Transfer/build flows, interruption and more filesystem races |
| Studio | PASS, 87 cases | PASS: unchanged page, HTTP/1.0, request gates, metadata/settings, estimates/routing, private uploads/files, planner refusals, synthetic CPU jobs/batches, anonymous prompt FDs, validated CPU profiles/tuning and failure/activity state | Pixel operations, full edit/upscale/refine, Oxi/NPU/image-slot adapters and more ownership edges; deferred behind cloud-only |
| Staged update/rollback | PASS, 15 scenarios / 22 CLI goldens | PASS: generic manifest transactions, sealed/bound hashes, exact restore, created/removed files, private state, receipts, positive identity, agent refresh and owner-edit refusal/recovery | Generated runtime materialization, interruptions and more filesystem races |
| Uninstall | PASS, 27 cases plus original empty-install CLI cases | PASS: exact journal/stage records, hash/owner-edit protection, dry-run/apply, positive stops/restoration, models kept by default, explicit model deletion, symlinks/parent escape, private-home refusal and partial failures | More concurrent record/filesystem changes and filesystem failure cases |
| Shipped scans | PASS, 38 cases | PASS: rules/order, machine words, exemptions, Unicode limits, generated trees, structured redaction/hashes, numeric values, directories, refusal before writing and layout-preserving scrubbing | More malformed inputs and filesystem races |
| Fake engine benchmark | PASS, 100 requests | PASS: matching upstream payloads and 101 calls including warmup, source/harness/binary identities, RSS/CPU/latency receipts | Concurrent/streamed measurements; hardware A/B belongs to the owner |

No hardware calls or real cloud API calls are part of this suite. Hardware A/B
belongs to the owner's lane.

At this reference SHA the gateway and cloud-only changes are still unmerged.
Do not port the earlier gateway in detail. Establish the reference suite from
merged main, then make the Rust cloud-only path complete and conformance-green
before hardware A/B. No empty suite or skipped test represents this pending work.

Bootstrap contracts use the real source payload in a throwaway prefix, compare
complete unit text, execute installed CLI help without PYTHONPATH, and verify
hash-bound rollback plus refusal for altered identity, collisions and symlinks.
Python's original units invoke `/usr/bin/python3`. The native snapshots change
only ExecStart to the serve/studio entries, retaining every other byte of the
service names, paths, resource limits and hardening settings.

Helper entry points can be replaced independently when the native implementation
provides them. Each override is a shell command string parsed with shlex:

| Environment variable | Python reference process |
| --- | --- |
| `OXC_BOOTSTRAP_COMMAND` | `python3 -m snapdragon_ai_lab.bootstrap` |
| `OXC_INSTALL_COMMAND` | `python3 -m snapdragon_ai_lab.bootstrap_install` |
| `OXC_EXPORT_RECEIPT_COMMAND` | `python3 lab/tools/export_receipt.py` |
| `OXC_SCRUB_RECEIPTS_COMMAND` | `python3 lab/tools/scrub_receipts.py` |
| `OXC_STUDIO_COMMAND` | `python3 lab/src/snapdragon_ai_lab/runtime/studio` |

Runtime CLI commands passed in overrides should be outside `/home` and `/tmp`,
which are private mounts in these tests (a build under the checkout works).

The staged update tests use public CLI processes, the real seal/installer/guarded
rollback tooling, a stateful systemd command peer and a loopback HTTP peer. They
cover check/install/explicit rollback, seal and live-data drift refusals, an
unproven service stop, and automatic recovery after chat or startup failure.
Progress output, errors and exit codes are golden-tested; only the measured chat
duration and temporary HOME path are normalized.

Run the model-free benchmark separately (not part of CI's parity gate):

```sh
python3 conformance/benchmark.py --requests 100 --idle-seconds 1
```

`OXC_DAEMON_COMMAND` selects another controller. RSS and CPU come from the actual
daemon PID, excluding bubblewrap and the synthetic engine/client. CPU percentages
are relative to one CPU core and are limited by the reported kernel clock-tick
resolution. Load RSS is sampled every 20 ms; latency covers complete sequential
HTTP requests. The stored Python baseline is an executor measurement with the
synthetic host files and fake engine, not laptop hardware validation. See
[benchmarks/python-a1c683f.json](benchmarks/python-a1c683f.json).

CI policy: keep the PR draft and test locally while iterating. Draft checkpoints
run formatting and mandatory scans. Full PR CI starts when marked ready; the
merge queue runs full integration CI again. Request a manual run deliberately
with one workflow dispatch when needed; use one blocking `gh pr checks --watch`
rather than polling.

Current synthetic comparison: [Python](benchmarks/python-f8c482e-a874eac.json) and [Rust](benchmarks/rust-f8c482e-a874eac.json). Both verify 101 identical engine request bodies including warmup and the expected reply/usage. Idle RSS was 19,696/7,084 KiB, sampled load RSS 20,504/7,692 KiB and p50/p95 HTTP latency 52.881/54.680 ms versus 1.095/3.077 ms. CPU counters have 10 ms resolution. These are controller-only executor measurements; no model or hardware performance is implied. Earlier receipts remain unchanged.

During the v0.2.0 release hold, keep all migration PRs draft and continue local
work/main follow-up without review requests or full-CI dispatches. Post-release
review is through the orchestrator (Fable) and oxa.

The standalone CLI harness keeps the unchanged credential/proxy-free environment
allowlist and rebinds only the read-only checkout after hiding HOME/tmp roots. A
synthetic checkout under /home verifies hosted-runner portability without exposing
other home contents or requiring an engine fixture.
