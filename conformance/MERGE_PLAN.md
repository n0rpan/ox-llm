# Reviewable Rust cloud migration

Ported up to main `bbec7919fd54ddcce6de49c6c33264e50e9a7494`.
Main is incorporated with normal merges. Never rebase or force-push a published
branch. Directory/package names follow main: `oxc/`, `oxc`, applicable `OXC_*`
variables; retained `LOCAL_LLM_*`, cache paths and schema identifiers keep their
reference meanings. Python remains the behavior reference.

#28 is a draft integration/proof branch and must not be merged as a monolith.
The other lane owns #177, #186 and #197; do not push their branches. #177 has
landed on current main. New work uses `rust-oxc/cloud-<name>` branches, separate
PRs against main and roughly 1,500 lines per slice, including their own contracts.
Do not push a queued PR; put follow-ups in a new small PR. Never merge our PRs.

## Cloud-only first

The target has no local engine installation, construction, qualification, model,
memory admission or inference lease use. Owned mocks and a foreign held lease
prove this on Linux x86_64/arm64 without Snapdragon. Hardware A/B waits until the
complete cloud-only suite is green. Do not change Python behavior to match Rust.

| Slice | Scope | Required conformance |
| --- | --- | --- |
| a. Discovery metadata | [#294](https://github.com/oxstack/ox-llm/pull/294), head `82beba1`: opt-in `oxc-gateway` library, empty defaults; model/chooser rows, model limits, health and startup line. No installed binary or full HTTP adapter. | Eight Python HTTP/native library cases: three providers, cloud off/default/offline, ignored local profile, invalid entries; held foreign lease, no config mutation or inference request. PASS. |
| b. Policy refusals | [#297](https://github.com/oxstack/ox-llm/pull/297), head `2dcbdd0`, 732 lines; cloud credentials unavailable, offline, budget exhausted/missing, unknown price and LAN guests refused before forwarding. | Exact public errors, zero provider requests and no local admission. |
| c. Secrets and redaction | Private request-time files, ownership/mode/no-follow checks; JSON and split-stream redaction. Never expose keys or credential paths in errors/ledgers. | File rotation and malformed/symlink/permission cases, escaped and split secrets, metadata/tool deltas. |
| d. Accounting | Daily cap, atomic reservations, conservative missing-usage/cancel settlement, private remote ledger and cache read/write pricing. | Concurrent cap, failures, ledger health/privacy, seeded spend and explicit 95% cache-hit prices (#283). |
| e. OpenRouter | Privacy preferences, JSON/SSE/tools, no paid retry, bounded responses and cancellation. | Owned loopback upstream, exact request/reply frames and settlement. |
| f. OpenAI | Supported Chat and Responses models, translations, long tool loops and identity-bound opaque continuation. | Main #251/#246 closed-history replay, current open call, missing/expired state and credential/history binding; JSON/SSE/tools/cancel. |
| g. Anthropic | Native Messages protocol, thinking/signatures, tools and cache controls, usage/cache-write settlement. | Owned native Messages mock, exact translations/frames, long thinking/tool history and cancellation. |
| h. Installation/integration | Engine-free install, native launcher/config/optional unit and full authenticated HTTP composition. Still opt-in. | Install/start/stop, existing config, force, symlinks, quoting, preserved credentials, no local model/engine or Python fallback. Complete cloud-only gate. |
| i. In-process library | Same gateway policy, secrets and accounting in oxa and `oxc serve` (#289). | Identical engine-free contracts in both adapters; packaging/retirement is later. |

Independent library/contract work can proceed while earlier slices are reviewed.
Keep subsequent dependent work local or draft until its prerequisite is on main,
so each final PR diff stays small. Do not make unrelated slices dependencies of
the cloud path. Existing transport extraction #271 remains a separate draft.

## Reference and test discipline

The current integration inventory has 109 methods assigned exactly once. New
Python contracts cover nine catalog, 24 gateway wire/accounting and 20 installer
scenarios. Those are subsets, not complete native cloud parity. Full cloud-only
and gateway gates must not pass as empty suites or silently fall back to Python.

The harness uses `/usr/bin/python3`, isolated HOME/environment, fixed CPU
frequency/thermal/memory counters, usage-wrap normalization and synthetic peers.
The Python Anthropic SDK is pinned to the reference requirement; an explicitly
supplied dependency directory is mounted read-only through private namespaces.
All cloud captures have private networks, including the public-price background
thread, and never read a real credential or contact a public API. No sign-in.

```sh
/usr/bin/python3 conformance/run_suite.py all --list
OXC_CONFORMANCE_PYTHON_DEPS=/absolute/pinned/dependencies /usr/bin/python3 conformance/run_suite.py cloud-catalog --python
OXC_CONFORMANCE_PYTHON_DEPS=/absolute/pinned/dependencies /usr/bin/python3 conformance/run_suite.py cloud-gateway-wire --python
OXC_CONFORMANCE_PYTHON_DEPS=/absolute/pinned/dependencies /usr/bin/python3 conformance/run_suite.py cloud-installer --python
/usr/bin/python3 conformance/run_suite.py cloud-installer --native /absolute/native/oxc
```

Local validation precedes ready status. Draft pushes run cheap formatting and
mandatory scans; mark a slice ready when its conformance subset is green. Never
poll CI: use one blocking checks watch if waiting, or continue other work.
Read automated review comments at each task boundary and answer them. The review
on #294 at `82beba1` reported no findings and has been acknowledged.

Follow main daily and at slice boundaries, using normal merges. Capture changed
Python contracts before porting changes. Keep this SHA and #28's first body line
current. The immutable synthetic benchmarks remain historical evidence only;
they do not demonstrate hardware or cloud parity. Local engine/Studio adapters
and default installation remain deferred behind cloud-only completion.

The draft integration runtime now composes the discovery/policy library in a
separate cloud-only listener that never constructs `engine::Manager`. Nine
catalog and eight public refusal cases pass natively; the latter checks errors
and zero forwarding only. Remote ledger rows and positive forwarding remain
unimplemented. This proof wiring is not a merge-sized PR.
