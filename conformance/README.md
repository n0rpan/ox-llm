# oxc drop-in conformance

Reference: main `a1c683f9e3783fdac4716bef53241833babe60a6`.
This is the initial contract slice, not a complete parity claim. No Rust
implementation has been started: the full Python suite must pass first.

Run from the repository root:

```sh
python3 -m unittest discover -s conformance -v
```

The suite invokes the shipped Python `runtime/local-ai` entry script under the
public executable name `oxc`, and the unmodified controller daemon as a separate
process. Each invocation gets a temporary HOME and explicit XDG directories; its
environment is constructed from an allowlist and inherits no API credentials or
proxy settings. HTTP connections target loopback only. Parser cases stop before
command execution, so they do not contact desktop applications or systemd.

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
These behaviors are preserved without changing Python.

Intentional reference refresh, after reviewing changes on main:

```sh
python3 conformance/test_cli.py --record-python
python3 conformance/test_http.py --record-python
```

Recording rejects alternate implementation command overrides. Review the diff;
do not refresh goldens merely because an alternate implementation fails them.

| Area | Python | Rust | Coverage still required |
| --- | --- | --- | --- |
| CLI parsing | PASS, 56 cases | Not implemented | Runtime behavior of every command, retired commands, option combinations |
| HTTP empty install | PASS, 7 cases | Not implemented | Configured daemon with fake engine, streaming, tools, logprobs, n, cancel, queueing |
| Cloud gateway | Not covered | Not implemented | All three providers, loopback mock upstreams, reservations, budgets, ledger, redaction, offline, unknown prices |
| Files | Not covered | Not implemented | Bidirectional config/state/ledger/secrets compatibility |
| Installation | Not covered | Not implemented | Bootstrap, update, rollback, uninstall, unit text, receipt and secret scanners |
| Fake engine benchmark | Not measured | Not implemented | Idle/load RSS, CPU and latency |

No hardware calls or real cloud API calls are part of this suite. Hardware A/B
belongs to the owner's lane.
