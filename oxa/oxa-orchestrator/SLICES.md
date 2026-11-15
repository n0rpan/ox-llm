# Landing the orchestrator after v0.2.0

Current coordinator steering: until v0.2.0 ships, retain at most four draft
PRs, do not mark ready, request reviews or poll hosted checks. Continue local
acceptance on the rolling branches. The earlier independent-slice landing plan
below is preparatory history, not authorization to open or ready more PRs.

The release core consolidation retains the complete tested preparation chain
through `84b745a` and merges the exact heads of #368 (`cf7a72b`) and #396
(`2258851`), including their main ancestry. Overlapping core files retain the
newer implementations: owned workspace jobs include Git preparation and worker
pipes retain their bounded nonblocking implementation. Those two step PRs are
superseded by this draft; their branches and commits remain available. Historical
#88, background/goals #129 and assembled CLI/TUI #176 remain intact. No code,
review clearance, merge or provider acceptance is inferred from consolidation.

Preserved sources: #88 at `ee04d55`, #129 at `f89ba2e`, and #176 at `0be9968`.
Protocol #22 is merged. Main now uses `oxc/` and package `oxc`; later controller
integration must use those names. The source rollups stay intact until all of
their carried behavior has a replacement. Historical source-clear assessments
are not blanket approval for newly adapted slices.

## Core/recovery (#88), dependency order

1. Private audit journal and controller lease (this slice): explicit default-off
   storage API, bounded durable records, private create-only paths and ownership.
2. Approved source snapshot/path validation, then private copy grants and bounded
   result collection. Keep source/application authority separate.
3. Bounded owned process/pipe transport, then native RPC supervision, cumulative
   budgets, control admission and verified cleanup. Separate transport and
   scheduling changes wherever needed to keep each diff within the line limit.
4. The missing A2A collector and loopback server components, then native A2A
   backend and fan-out/fan-in scheduling with attributed partial results.
5. Bounded inspection of retained journals, then explicit original-agent
   reconciliation. No automatic replay or recorded-PID signaling.
6. Checkpoint/consent, queued resume and lineage as separate tested additions.
7. Optional Git worktree snapshots, exact approved candidate publication and
   independent agent registration/hosting, split by their separate authority
   boundaries and each under the line limit.
8. Minimal default-off CLI entrypoints for the completed library capabilities.

Future independent landing requires dependencies on main. Until release, retain
the prepared dependency sequence in the core rollup and test locally; do not
modify queued branch commits. Small source modules are not an excuse to omit
negative cases, publication ownership or actual-process cleanup evidence.

## Goals/background (#129), then CLI/TUI integration (#176)

Preserve the #129 review finding as required work: settlement-induced pause or
budget exhaustion and failed turn admission must revoke already-running owned
tasks, while a legitimate blocked/waiting state preserves them. The owning slice
must include the requested real-TUI Esc and settlement/admission regressions with
confirmed reap and absence of delayed side effects. Existing passing socket
Pause/Cancel/deadline cases are positive controls, not substitutes.

Then split session goal state/budgets, shared-session controls, background tasks,
worker profiles/wakes, TUI/CLI surfaces and accounting/compaction in dependency
order. Keep the native JobRegistry release workaround and post-release bridge
issue #258 explicit.

The live smoke's 512-token worker output ceiling must be fixed in its owning
worker/request-budget slice, not silently raised in an external driver. Preserve
separate overall token/time/output budgets, expose the configured per-response
allowance, account length-limited turns honestly, and prove truncation handling
with owned mocks before another live experiment. The prior live authorization
was one-shot and has been consumed; this slice plan authorizes no paid rerun.

Each rolling draft records its scope, dependencies, current-base adaptation,
review tier, default-off hooks and local acceptance commands/results. Hosted
checks are not polled during release mode. No full-evening or provider-billing claim follows from mock tests.
