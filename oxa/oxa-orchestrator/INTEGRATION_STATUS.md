# Issue #94: current-main integration checkpoint

PR #436 remains a draft. This is a partial integration of #176, not the
assembled CLI/TUI or working-evening acceptance.

## Carried

- Existing default-off `core` preparation remains intact.
- The saved merge `e8900a11` retains source ancestry through `e1d640da`
  and carries the session goal state machine behind a new default-off
  `interactive-control` feature, which enables `core`.
- Goals preserve cumulative wall/turn/token accounting, observed token excess,
  explicit completion evidence, revision checks, pause/settlement and redirect.
- Event pagination returns the next unread cursor separately from the ledger
  high-water mark. A failing-first public-API regression reproduced skipped
  events (expected cursor 1, received 3); it now passes, alongside coverage of
  bounded pages, retention gaps and empty pages.
- Runtime-only state overrides are deliberately not exposed without their
  owning runtime. There are no CLI/TUI, listener, worker admission or normal-work
  restrictions in this slice.

## Still to integrate

The assembled #176 head `b83d0e33` is **not** an ancestor of this checkpoint.
Continue through normal merges with explicit default-off hooks, preserving the
newer core implementations: shared session/control transport, owned background
commands and cancellation/settlement, worker lanes, exact review handoff,
CLI/TUI wiring and their real loopback/tmux acceptance tests remain outstanding.
The existing merge ancestry is not evidence those features are present.

Controller code already lives in `oxc/` with package `oxc`; there are no tracked
files under `lab/`. This slice changes no Python code or optional SDK packaging.
Resolve the optional SDK bootstrap route with the coordinator before carrying
those changes; frozen qualification pins, scanners and allow-lists stay intact.
No hand-copy of old controller files is needed or authorized.

## Local validation (2026-11-15)

Using Cargo on PATH, all locked/offline where applicable:

- `cargo fmt --check`: pass.
- `cargo clippy -p oxa-orchestrator -- -D warnings`: pass.
- `cargo clippy -p oxa-orchestrator --features interactive-control --all-targets -- -D warnings`: pass.
- `cargo check -p oxa-orchestrator --lib --no-default-features`: pass.
- `cargo build -p oxa-orchestrator --features interactive-control`: pass.
- Feature-enabled goal unit tests: 8 pass; public pagination integration: 1 pass.
- Unchanged `python3 ci/check_net_surface.py --self-test`: clean, all eight planted violations caught.
- Full crate tests were run both default (55 pass, 86 fail) and feature-enabled
  (63 pass, 86 fail). The existing filesystem suites refuse the sandbox's
  overflow-UID-owned root directory before exercising their normal paths.
  This is tracked by #363 / draft #372. A private checkout-local TMPDIR does
  not fix root ownership. No namespace check or test was weakened or skipped.
  A host-capable full rerun is still required; this checkpoint is not all green.

No PR comments, inline review comments or submitted reviews were returned by
GitHub for #436 at this checkpoint, so there was no VERDICT finding to resolve.
No hosted-green claim, paid request, sign-in, gateway change, push, ready
transition, queue action or merge was performed.
