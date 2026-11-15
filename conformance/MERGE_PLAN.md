# Reviewable Rust controller migration

Ported up to main `ef6d54c8ff2882b3503e3b168c08345590ffe97c`.
Last main review: 2026-11-15 UTC. The latest merge retires one-off stage entry points, retains their shared
helpers and updates the retained v0.2.0 stager to use them. Controller runtime
files are unchanged. The accompanying engine changes are formatting-only and
add its hosted formatter gate, retained alongside oxa and oxc. Subsequent
merges update oxa/UI/CI, declare done checks and add a design-only OAuth document;
controller behavior is unchanged. OAuth remains pending project registration.

## Priority and source of truth

Cloud-only means **no local engine**: no engine/model installation, launch,
preload, inference lease acquisition, local qualification or host-memory
admission on the remote request path. The first complete A/B target is this
mode on Linux x86_64 and arm64, using loopback mock providers and synthetic keys.
Hardware A/B waits until the cloud-only reference and Rust suites are green.

On this main, #24 and the cloud-only stack (#78, #79, #80, #82, #84) are still
unmerged. #81 is an unrelated orchestrator PR; #83 is not a pull request.
Do not port the earlier gateway implementation. After #24 lands, capture its
actual HTTP/file/process behavior first, then port it. Repeat for the cloud-only
stack as it lands; branch-only behavior is not a frozen main contract.
Studio and remaining local-engine adapters follow cloud-only completion.

## How the code will land

#28 remains the draft integration/proof branch, not a merge-sized change.
Use only `rust-oxc/*` branches. Extract each slice into its own draft PR based
on main or the immediately
preceding slice. Do not hide the entire existing port behind the first PR's flag.
Each slice must build independently with only its necessary modules, fixtures
and goldens. Split coupled modules such as CLI execution, daemon dispatch and
proxy finalization before extracting them. No temporary Python fallback.

The initial PR introduces an opt-in `native-controller` Cargo feature with no
default features and a separate native binary requiring that feature. Existing
installation scripts and service ExecStart lines keep using Python. Subsequent
PRs extend that binary; enabling native installation is a final, separately
reviewed change after complete parity. Unsupported native commands must refuse
explicitly until their slice lands. No existing user path switches by default.

Aim for roughly 500–2,000 handwritten product lines per PR; a larger slice must
be split at its behavior boundary. Generated Unicode tables and immutable JSON
goldens get separate file/provenance summaries rather than inflating the product
diff. A PR includes only its own conformance subset and direct dependencies.

| Order / separate PR | Scope and dependency | Conformance gate |
| --- | --- | --- |
| 0. Contract tooling | Isolated process harness, slice inventory and reference provenance. No product behavior changes. | Inventory accounts for every captured method exactly once; reference subsets pass. |
| 1. CLI grammar and config | Parser extraction is draft [#177](https://github.com/oxstack/ox-llm/pull/177); config follows separately. Binary scaffold; parser/help and path/config reader/writer, separated from command execution. T2. | `test_cli`; config preservation/mode/symlink contracts moved out of engine-dependent tests. |
| 2. CLI execution | Bounded HTTP client, status projection and desktop exec handshake, depending on 1. T2. | `cli-config`; synthetic HTTP/system peers, no installed engine. |
| 3. HTTP transport and gates | Framing, limits, Host/Origin/CORS/auth, request IDs and engine-free dispatch, depending on 1. T3. | `http-foundation`; gate subset of `http-admission` adapted to no-engine fixtures before extraction. |
| 4. Request records | Private HMAC identity/files and bounded usage records, separated from local engine bookkeeping, depending on 3. T3. | File/privacy cases from `ledger`; remote settlement cases captured after #24. |
| 5. Cloud-only catalog and policy | Main's remote catalog/capabilities, cloud-off/offline rules, status/health with no engine, depending on 3. T3. | New Python contracts after #24/#78; no fake engine or weight files; held foreign lease remains untouched. |
| 6. Cloud credentials and spend | Request-time private secrets, redaction, reservations, daily limits, unknown prices and settlement, depending on 4–5. T3. | New gateway refusal/concurrency/cancellation and file interoperability contracts after #24. |
| 7. OpenRouter / OpenAI Chat | Exact upstream request/reply and SSE/tool/usage behavior, depending on 6. T3. Split by provider if needed. | Loopback provider goldens; request counts prove no paid retry or local fallback. |
| 8. OpenAI Responses | Continuation state/binding/expiry, history recovery and streaming, depending on 7. T3. | New #24 continuation/tool-history/identity contracts; fresh-turn and unresolved-state refusals. |
| 9. Native Anthropic Messages | Native protocol mapping, thinking/cache/tool streaming and errors, depending on 6. T3. | New #24 mock Messages contracts. Native Rust HTTP adapter, no Python runtime dependency. |
| 10. Cloud-only installation | Engine-free native bundle, config and optional service, depending on 7–9. T3. Still opt-in. | New #79/#84 install/start/stop/private-file round trips; no models, local binaries or credentials copied. Complete `cloud-only` gate. |
| 11. Local lifecycle | Lazy startup/lease/auth/session, guard, cancellation/reaping and local admission, depending on 3–4. T3. Separate guard/progress hooks if needed. | `engine-lifecycle`, local `http-admission`, numeric engine ledger. |
| 12. Cache and harness | Storage readers, config controls, cache mutation and engine-owned adapters, depending on 11. T3. Separate read-only, mutation and harness PRs. | Respective modules in `cache-harness`; Python/Rust/Python file cases. |
| 13. Sharing | Keys/config first, then network/TLS listeners and shared admission/finalization, depending on 11. T3; three PRs. | Respective modules in `auth-sharing`, including real I/O deadlines. |
| 14. Receipt tools | Native scanner/exporter/scrubber, Unicode facts and provenance; independent of engines. T3. | `receipt-tools`, unchanged original repository scanners and old goldens. |
| 15. Local installer | Descriptor-anchored transaction primitives first, then native bootstrap/bundles, depending on 11 and later Studio completion. T3; two PRs. | `installer`; exact units except explicit native ExecStart, unchanged size bound and journal interoperability. |
| 16. Updates and uninstall | Separate update/recovery and deletion PRs on the transaction primitives. T3. | Their respective modules in `updates-uninstall`, owner edits, refusal and rollback round trips. |
| 17. Studio | Metadata/page/files, then CPU jobs/pixel operations, then Oxi/NPU IPC; separate PRs after cloud-only. T3 for ownership/files/IPC. | `studio`, with separate case sets as each path is completed. No SDK/device/model material. |
| 18. Default switch | Installation/runtime selection only after remaining parity gaps are closed and owner readiness review. T3. | All conformance, required CI, rollback proof; hardware lane stays with owner. |

## Runnable gates and pending contracts

`slices.json` assigns every currently captured unittest method to one review
subset. These are extraction/verification groups, not a claim that each coupled
module can be copied unchanged into a new PR. `run_suite.py` audits that inventory
before running and sets **all seven** command overrides for a native run.

```sh
python3 conformance/run_suite.py cloud-foundation --python
python3 conformance/run_suite.py cloud-foundation --native /absolute/path/to/oxc
python3 conformance/run_suite.py engine-lifecycle --native /absolute/path/to/oxc
python3 conformance/run_suite.py all --list
```

`cloud-foundation` currently checks CLI behavior and engine-free HTTP gates. It
does not prove cloud-only mode. `cloud-only`, `cloud-gateway` and `cloud-installer`
exit with an explicit pending-baseline error instead of passing empty suites or
skipping tests. Replace those errors with real captured contracts after the
corresponding main changes land. The completed cloud-only profile must cover
all three upstream protocols, JSON/SSE/tools, Responses continuation, discovery
and health, cloud off/offline, request-time key refresh/refusals, reservations and
settlement, ledger privacy, cancellation, install/start/stop and absence of local
engine activity. Every upstream is an owned loopback mock; no real key is read.

Follow main daily during active work and before each slice is readied. Merge
normally, review controller/test/config changes, capture changed contracts before
adjusting Rust, and update the exact SHA above, the parity matrix and #28's first
line. Keep slices draft while iterating; local checks precede pushes. Draft CI
runs formatting/scans. Request full CI deliberately when a slice is reviewable,
and use one blocking checks watch if waiting is necessary. Nothing merges here.

Release hold: until v0.2.0 ships, keep all migration PRs draft, request no
reviews/full CI and continue local implementation and main follow-up. After the
release, reviews go through the orchestrator (Fable) and oxa, not Codex.
