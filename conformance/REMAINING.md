# Remaining conformance work before Rust

Reference main: `a1c683f9e3783fdac4716bef53241833babe60a6`.
The passing suite is partial. No Rust implementation has been started.

The following gaps remain visible rather than being represented by skipped tests:

- Cloud routing, three upstream providers, budget reservations, unknown-price
  refusal, remote ledger labels and redaction: no implementation exists on this
  reference main. The user has been asked whether to wait for main or implement
  the planned Rust functionality as a new feature after the existing contracts.
- Disk state/prefix/session caches: seed actual on-disk formats, budget/GC, clear
  scopes, preload, concurrent mutation and additional cross-implementation reads.
  Current tests cover an empty store, budgets and config persistence only.
- LAN sharing: actual listener configuration, key rotation and file permissions,
  origin/route/header restrictions, rates, shared/local priority and preemption.
  All listeners/upstreams used in tests must remain synthetic and isolated.
- Lifecycle: pressure-based keep-warm, crashed engine/reaper recovery, host
  admission/memory guard variants, queue/replay flags, background preemption,
  CPU pinning and Flash-specific launch adapters/capabilities.
- Images: the CLI dispatch is covered; the shipped controller's image-slot IPC
  and related runtime paths need contracts without SDK, device or model content.
- CLI: exercise remaining positive/negative branches (qualified Flash/plan,
  populated ledger/digest, valid cache status, sharing keys, offline failures).
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

After coverage is complete and passes Python, create `oxc-rs/` (safe Rust,
Tokio plus axum/hyper) and port each area against this same suite. Complete both
file directions and the fake-engine benchmark with Rust before any readiness
claim. Hardware A/B remains the owner's responsibility.

Fetch/merge main every few hours and before any completion claim. Review all
`lab/` changes since the previous merge point, add reference contracts first,
then port the changed behavior. Keep the draft PR's first line and parity table
in sync. Never merge, tag, release, push main or mark hardware readiness early.
