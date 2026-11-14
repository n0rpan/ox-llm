# oxc drop-in conformance

Reference: main `a1c683f9e3783fdac4716bef53241833babe60a6`.
This is the initial contract slice, not a complete parity claim. No Rust
implementation has been started: the full Python suite must pass first.

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

Configured HTTP tests launch the unchanged daemon and a hash-pinned synthetic
engine executable through its real lifecycle. A private bubblewrap mount/cgroup
namespace supplies deterministic host memory, PSI and cgroup counters, including
the installed service's `memory.swap.max=0`. Changing those fixture files tests
the real guard's refusal and recovery. No runner cgroup settings are changed.
The synthetic `/proc` view delegates other metadata to live procfs and exposes
engine PIDs before readiness. The daemon receives SIGTERM directly for graceful
shutdown, rather than terminating the bubblewrap supervisor.

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
python3 conformance/test_http.py --record-python
python3 conformance/test_api.py --record-python
python3 conformance/test_http_gates.py --record-python
```

Recording rejects alternate implementation command overrides. Review the diff;
do not refresh goldens merely because an alternate implementation fails them.

| Area | Python | Rust | Coverage still required |
| --- | --- | --- | --- |
| CLI parsing | PASS, 56 cases | Not implemented | Runtime behavior of every command, retired commands, option combinations |
| HTTP empty install | PASS, 7 cases | Not implemented | Additional setup failures |
| Configured HTTP | PASS, 50 cases | Not implemented | More option combinations and engine error paths |
| HTTP gates | PASS, 17 cases | Not implemented | LAN share listeners and rate limits |
| Lifecycle | PASS, 11 tests | Not implemented | Pressure keep-warm, image handoff, crash/reaper paths, CPU pinning |
| Files | PASS, 3 tests | Not implemented | Additional state stores and sharing secrets |
| Cloud gateway | Not covered | Not implemented | All three providers, loopback mock upstreams, reservations, budgets, ledger, redaction, offline, unknown prices |
| Cross-implementation file reads | Harness ready; Python/Python PASS | Not implemented | Run Python/selected/Python round trips with Rust |
| Installation | Not covered | Not implemented | Bootstrap, update, rollback, uninstall, unit text, receipt and secret scanners |
| Fake engine benchmark | Not measured | Not implemented | Idle/load RSS, CPU and latency |

No hardware calls or real cloud API calls are part of this suite. Hardware A/B
belongs to the owner's lane.

At this reference SHA there is no cloud gateway implementation in the runtime:
Wave D in AGENTS.md describes future work. Cloud provider/budget tests cannot yet
be validated against Python. This remains an explicit baseline gap; neither
cloud conformance nor full suite completion is claimed.
