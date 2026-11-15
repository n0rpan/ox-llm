# Remaining conformance and Rust work

Reference main: `f8c482ea3936774c0eb6310cfe1d8e5415ae67e8`.
The passing Python suite is partial. Per the owner's updated instruction, Rust
implementation has started in parallel with the remaining contracts. See
`oxc-rs/README.md` for the implemented scope and local checks.

The following gaps remain visible rather than being represented by skipped tests:

- Cloud routing, three upstream providers, budget reservations, unknown-price
  refusal, remote ledger labels and redaction: no implementation exists on this
  reference main. Implement the planned native functionality after the existing
  runtime paths, keeping the absent Python baseline explicit.
- Disk state/prefix/session caches: extend actual on-disk formats,
  scopes, preload, concurrent mutation and additional cross-implementation reads.
  Usage tests now read populated state formats, pending-write ages, overrides and
  symlink refusals. Budget persistence and controller eviction now cover TTL,
  value ranking, shared blocks, pins, opaque manifests and private lease ownership.
  Controller clear now covers all/model/session/hint scopes and shared ownership.
  PrefixStore lookup/save, engine-side persistence edge cases and further race
  coverage remain pending.
- LAN sharing: more IPv6, network/config/certificate refresh races, slow response
  readers, local/shared admission and finalization races, and offline CLI fallback.
  Thirty-one LAN/TLS cases now cover route/link discovery, persisted network pins,
  missing MAC/absent networks, auto-pause/resume, malformed discovery/bind failures,
  certificate/mismatch refusal, verified HTTPS/TLS 1.2 and TLS disconnect behavior.
  One scenario observes real 60-second stalled-header/body/handshake deadlines
  concurrently, including slot release and no engine startup.
  Sixteen active Tailscale-listener cases now cover authentication/key revocation,
  route/header/payload restrictions, rates/body limits, loaded-model admission,
  keyed remote ledger records, preemption before/after streaming headers,
  connection limits, off/rebind and closing existing connections.
  Fourteen loopback status/off cases and seventeen key lifecycle cases also pass,
  including private files, config preservation and Python/Rust lock/file reads.
  All listeners/upstreams used in tests remain synthetic and isolated.
- Lifecycle: pressure-based keep-warm, crashed engine/reaper recovery, host
  admission/memory guard variants, queue/replay flags, background preemption,
  CPU pinning and Flash-specific launch adapters/capabilities.
- Images: 27 Studio contracts now cover the unchanged page, HTTP/1.0, request
  gates, model metadata, settings and metadata estimates, executable readiness and hash-bound
  size-gate consumption. HTTP estimate/upload/file/job routes, generation planners, pixel
  operations, controller image-slot IPC and engine adapters still need contracts
  without SDK, device or model content.
- CLI: extend malformed metadata, argument combinations and filesystem failures;
  extend populated ledger/digest, valid cache status, sharing keys and offline
  failures. Fifteen desktop/model dispatch cases now cover detached sessions,
  inherited stdin/umask, exec/service failures, qualification and read-only planning.
  Sixteen status/code/mode cases cover filtered metadata, offline/stopping states,
  selected models, download phases and bounded trial queries. All forty-eight runtime CLI
  cases pass against both the Python and native daemons.
  Direct Oxe chooser/status facts now cover receipt-derived qualification,
  prefix identities, selection/load state, resident bytes, idle/unload availability
  and shared harness facts. Detailed failure provenance and additional status
  fields remain pending, along with the Flash/classic qualification adapters.
- Bootstrap: source transfer/build/check flows need loopback fixtures, never a
  model download or real API call. Native staging/install/rollback now covers 34
  scenarios: pinned plans, native bundles, exact unit controls, installed help and
  both service entries, the unchanged 8 MiB bound, create-only transactions,
  canonical lease preservation, owner edits, refusals, offline agent refresh and
  Python/Rust journal reads. Further interruption and filesystem races remain.
- Updates: generated runtime materialization, interruption recovery and additional
  filesystem/service races. Fifteen scenarios (22 CLI snapshots) now pass for
  generic manifest transactions, sealed/bound hashes, exact restore, created and
  removed files, private state/receipts, positive release identity, agent refresh,
  and refusal to overwrite owner edits during recovery. Legacy rollback bindings
  are parsed as quoted JSON data; no Python interpreter is invoked by the port.
  Generated stagers remain refused until their additional checks/materialization
  have native adapters.
- Uninstall: more record/filesystem mutation races, foreign ownership and
  filesystem failures. Twenty-seven cases now pass for exact bootstrap/stage
  records, hashes/owner edits, dry-run/apply, positive stop/restoration, default
  model retention, explicit model deletion, symlinks/parent escapes, home
  validation and partial failures. Systemd peers remain synthetic.
- Receipt scans: more malformed inputs and filesystem cases. Thirty-eight native
  cases now pass for exact rules/order, machine words/exemptions, Unicode limits,
  generated trees, argv/environment redaction, hashes, numeric/non-finite values,
  directory inputs, refusal before writing and layout-preserving scrubbing.
  Never add real secrets or SDK
  content, split strings to evade scanners, or weaken existing scans/exclusions.
- Installation units: the two ExecStart lines become native serve/studio entry
  points. Separate snapshots verify every other byte, including names, paths,
  resources and hardening controls. Bundles remain unqualified; no service starts
  during installation. Complete Studio behavior and transfer/build remain pending.

Continue `oxc-rs/` (safe Rust, Tokio plus axum) area by area against this same
suite. The owner authorized starting implementation while conformance/CI are
still in progress. Establish or extend Python contracts before adjusting a
ported behavior; do not change Python behavior to ease parity. Complete both
file directions and the fake-engine benchmark with Rust before any readiness
claim. Hardware A/B remains the owner's responsibility.

Fetch/merge main every few hours and before any completion claim. Review all
`lab/` changes since the previous merge point, add reference contracts first,
then port the changed behavior. Keep the draft PR's first line and parity table
in sync. Never merge, tag, release, push main or mark hardware readiness early.

CI policy from the owner: keep PRs draft during iteration and test locally.
Draft pushes run formatting and mandatory scans. When requesting a full public
run/review, mark ready deliberately or dispatch checks.yml once for the branch.
The merge queue runs full integration CI again. If waiting is needed, use one
blocking gh pr checks --watch; never poll CI status.

Next implementation step: complete Studio/image contracts and generated materialization. Twenty-seven digest cases now cover numeric/enum aggregation, text/origin
filtering, stored days, private writes, trend/soak thresholds, stale status, dates,
installed versions and file interoperability. Rust now passes all 62 configured API and all 18 HTTP gate cases.
All 18 lifecycle tests and 13 harness cases pass. The harness tests exercise
opt-in learning, memory-only operation, streaming, bounded candidates/prefixes,
prepare requests, compatibility counters and Python/Rust file reads.
The native request ledger passes 12 goldens and eight additional checks, including
private salt/hash/file compatibility, history/reply tracking, cancellation,
advisory failures and unload finalization. Optional turn diagnostics and
queue/replay modes remain pending. The native cache usage and storage inventory
readers pass 14 additional cases. No tests are skipped to hide these gaps.
All three config/ledger/key-file contracts now pass with Rust. Eight context-mode
cases cover published limits/capabilities, hash-bound RSS-proof gates, active
requests, cap-changing reloads and symlink/permission preservation. Twelve budget
cases cover persistence and descriptor-anchored eviction without reading KV bytes.
Eighteen cache-clear cases cover scope validation, shared/anonymous owners,
private durable manifest rewrites, lease/busy refusal, retained primers and
legacy Rust prefixes. Classic PrefixStore clearing preserves its private-directory,
pending/prune, recognized-name and fsync rules; symlink roots and invalid budgets
refuse the operation, and other clear scopes retain that store.
Nine engine-cache cases cover ownership, authenticated controls, bounded replies,
private overrides, budget zero, explicit/async preload, saved/in-memory session
hints, and immediate ownership release after engine death.
Seven legacy conversation-store cases cover separate/equal flags, private root
overrides from config/environment, and dropping stores/options when disk is off.
Twelve automatic engine-ledger cases now cover per-profile opt-in, an inherited
pipe separate from stdout, numeric/enum filtering, bounded lines, private rotation,
advisory filesystem failures, explicit ledger arguments and unload/reload drain.
More malformed numeric inputs and stop races remain uncovered. Full Rust conformance
remains FAIL, and no native deployment or readiness claim is made.
