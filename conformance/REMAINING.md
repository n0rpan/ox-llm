# Remaining conformance and Rust work

Reference main: `6e26fd4ff4a8d3fdcce2e9727907bff1a5b17234`.
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
- LAN sharing: actual listener configuration, key rotation and file permissions,
  origin/route/header restrictions, rates, shared/local priority and preemption.
  All listeners/upstreams used in tests must remain synthetic and isolated.
- Lifecycle: pressure-based keep-warm, crashed engine/reaper recovery, host
  admission/memory guard variants, queue/replay flags, background preemption,
  CPU pinning and Flash-specific launch adapters/capabilities.
- Images: the CLI dispatch is covered; the shipped controller's image-slot IPC
  and related runtime paths need contracts without SDK, device or model content.
- CLI: port digest, update/rollback and uninstall commands;
  extend populated ledger/digest, valid cache status, sharing keys and offline
  failures. Fifteen desktop/model dispatch cases now cover detached sessions,
  inherited stdin/umask, exec/service failures, qualification and read-only planning.
  Sixteen status/code/mode cases cover filtered metadata, offline/stopping states,
  selected models, download phases and bounded trial queries. Forty runtime CLI
  cases pass against the Python daemon. An integrated native run passes 34/40;
  status JSON, storage text/JSON and sharing status/JSON/off remain different.
- Bootstrap: source transfer/build/check flows need loopback fixtures, never a
  model download or real API call. Current tests cover source-payload staging,
  installation, unit text, installed help and rollback/refusal cases.
- Updates: created/removed files, interruption recovery, positive release identity
  checks, rollback refusing unrelated owner edits and exit-3 recovery failure.
  Current stages are identity-free synthetic stages; no release identity claim
  is made.
- Uninstall: real installed-prefix records, default model retention, explicit model
  deletion safety, changed files and partial failures. Current CLI fixtures have
  a synthetic runtime config and cover dry-run/apply only.
- Receipt scans: additional scanner rule inputs. Never add real secrets or SDK
  content, split strings to evade scanners, or weaken existing scans/exclusions.
- Installation units: Python ExecStart must become a native entry point while
  preserving names/paths/resource and hardening controls. Document the necessary
  executor change; do not silently relax unit comparisons.

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

Next implementation step: remaining CLI
commands. Rust now passes all 62 configured API and all 18 HTTP gate cases.
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
