# oxc drop-in conformance

Reference: main `d93f2c5fe0cc87c8fb7d3b33e28c90935280425a`.
The current suite passes 435 scenarios in 62 unittest methods. This remains a
partial contract suite, not a complete parity claim. The owner authorized starting Rust in parallel with the remaining conformance
work. The initial native crate is in `oxc-rs/`; full Rust parity is not yet claimed.

Requires Linux, Python 3.10+ and bubblewrap with unprivileged user and cgroup
namespaces. The lab CI job already installs bubblewrap. Run from the repository root:

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
part of the goldens. The fixed HOME path in that namespace is synthetic, making
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
python3 conformance/test_http.py --record-python
python3 conformance/test_api.py --record-python
python3 conformance/test_http_gates.py --record-python
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
python3 conformance/test_cli_actions.py --record-python
python3 conformance/test_cli_status.py --record-python
python3 conformance/test_catalog.py --record-python
python3 conformance/test_share_control.py --record-python
python3 conformance/test_share_keys.py --record-python
python3 conformance/test_scans.py --record-python
python3 conformance/test_update.py --record-python
```

Recording rejects alternate implementation command overrides. Review the diff;
do not refresh goldens merely because an alternate implementation fails them.

| Area | Python | Rust | Coverage still required |
| --- | --- | --- | --- |
| CLI parsing | PASS, 65 cases | PASS, captured grammar | More option combinations |
| CLI ledger files | PASS, 5 cases | PASS | Additional historical ledger labels |
| CLI runtime | PASS, 48 cases | Partial: 40/48 native CLI cases against both Python and native daemons | Digest, update/rollback and uninstall commands |
| CLI desktop/model dispatch | PASS, 15 cases | PASS: detached session and exec handshake, inherited stdin/umask, service failures, bounded model qualification/errors, selection and read-only planning | More malformed replies and command races |
| CLI status/code/mode | PASS, 16 cases | PASS: filtered metadata, offline/stopping states, selected-model dispatch, Flash download phases and bounded trial queries | More malformed metadata and clock races |
| Chooser/status metadata | PASS, 12 goldens and one additional check | PASS: direct Oxe chooser facts, selected/loaded states, prefix identities, receipt qualification/invalidation, resident/idle/unload and shared harness facts | Detailed failure provenance, Flash/classic adapters and more status fields |
| Sharing controls | PASS, 14 cases | PASS: loopback status/off, options, key metadata/privacy, busy inference, validation, config symlinks/permissions and absent setup | Active listeners, network discovery, authentication/preemption and offline CLI fallback |
| Sharing keys | PASS, 17 cases | PASS: show once, rotation, private atomic files, real cross-process locks, malformed/public/symlink refusal, overrides and Python/Rust file/lock interoperability | Active-listener revocation and more filesystem failures |
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
| HTTP gates | PASS, 18 cases | PASS | LAN share listeners and rate limits |
| Lifecycle | PASS, 18 tests | PASS | Pressure keep-warm, image handoff, crash/reaper paths, CPU pinning |
| Harness registry | PASS, 13 cases | PASS: learning/privacy, controls, prepare, streaming, limits and Python/Rust file reads | More malformed/advisory-I/O cases |
| Request ledger | PASS, 12 goldens and 8 additional checks | PASS: private HMACs, JSON/SSE records, history/reply tracking, errors/cancel, retention, limits, engine hints, advisory I/O and unload finalization | Optional turn diagnostics, queue/replay modes and more malformed backend timings |
| Files | PASS, 3 tests | PASS: config and ledger Python/Rust/Python, API-key refresh/fail-closed | Additional state stores and sharing secrets |
| Cloud gateway | Not covered | Not implemented | All three providers, loopback mock upstreams, reservations, budgets, ledger, redaction, offline, unknown prices |
| Cross-implementation file reads | Harness/ledger ready; Python/Python PASS | Harness and ledger Python/Rust/Python PASS | Other state stores |
| Bootstrap/install | PASS, 4 scenarios | Not implemented | Transfer/build/install-journal edge cases |
| Staged update/rollback | PASS, 6 scenarios / 9 CLI goldens | Not implemented | Release identity proof, created/removed files, interruption recovery |
| Uninstall | PASS, dry-run and apply CLI cases | Not implemented | Installed release records and model deletion cases |
| Shipped scans | PASS, 11 cases | Not implemented | More secret/SDK rule inputs (no real secrets or SDK material) |
| Fake engine benchmark | Python baseline recorded | Not implemented | Repeat identical measurement with Rust |

No hardware calls or real cloud API calls are part of this suite. Hardware A/B
belongs to the owner's lane.

At this reference SHA there is no cloud gateway implementation in the runtime:
Wave D in AGENTS.md describes future work. Cloud provider/budget tests cannot yet
be validated against Python. This remains an explicit baseline gap; neither
cloud conformance nor full suite completion is claimed.

Bootstrap contracts use the real source payload in a throwaway prefix, compare
complete unit text, execute installed CLI help without PYTHONPATH, and verify
hash-bound rollback plus refusal for altered identity, collisions and symlinks.
Python's current unit invokes `/usr/bin/python3`; replacing that ExecStart with
the native entry point will need a documented migration delta while retaining
its service name, paths, resource limits and hardening settings.

Helper entry points can be replaced independently when the native implementation
provides them. Each override is a shell command string parsed with shlex:

| Environment variable | Python reference process |
| --- | --- |
| `OXC_BOOTSTRAP_COMMAND` | `python3 -m snapdragon_ai_lab.bootstrap` |
| `OXC_INSTALL_COMMAND` | `python3 -m snapdragon_ai_lab.bootstrap_install` |
| `OXC_EXPORT_RECEIPT_COMMAND` | `python3 lab/tools/export_receipt.py` |
| `OXC_SCRUB_RECEIPTS_COMMAND` | `python3 lab/tools/scrub_receipts.py` |

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
