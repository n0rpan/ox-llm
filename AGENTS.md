# Working on ox-llm as a coding agent

This file is for AI coding agents (and the people who run them) that open pull requests on this
repository. Human contributors: see [CONTRIBUTING.md](CONTRIBUTING.md); everything below applies to you too.

You have **no access to the maintainer's Snapdragon laptop, its NPU or its models**. Everything here is
code, tests, CI and docs. Hardware validation (speed, memory, long runs) is done separately by the
maintainer, who says in the PR when a hardware check is needed.

## Review guidelines

- Never expose credentials, tokens or private data in prompts, errors, Debug, logs or tests;
  use synthetic fixtures. Do not open real credential files during review.
- Keep engine transport selection, native binary/launch configuration, `allowRemote`, share
  authority, login/token paths and `oxa.inbox.headless` user-only. A project file must never override these decisions.
- Offline mode makes no network calls; do not add provider, discovery or HTTP fallback paths.
- Preserve sandbox/bwrap enforcement and file-tool denials for private paths. Only explicit
  owner-selected yolo deliberately disables the permission floor. Off Linux there is no OS sandbox
  (owner policy, #1512): in-process guards stay, bash gets the same cleared base environment, and
  the bash network is not restricted there; the start notice and the model env note say so.
- Preserve durable ledger/usage settlement, uncertainty reservations and configured budget caps
  across retries, cancellation, concurrent processes and restarts. Every metered request is recorded
  in the ledger (an unknown cost is recorded as unknown). Limits are owner configuration: with no cap
  configured there is no cap check, and a missing price or usage field is logged and recorded, not
  refused. Do not flag the absence of a block the owner has not configured. This applies to
  authenticated, allowlisted share guests too: their authority, tokens and request quotas stay, an
  uncapped metered spend by guests is shown as a clear warning in the owner UI, not refused.
- OpenCode Go reserves subscription tokens only, including when the provider falls back to its
  paid balance. `oxa.cloud.daily_budget_usd` applies to metered API admission. Record actual Go
  charges reported by the provider in the ledger and include them in the shared daily spend.
  Preserve subscription token accounting and provider-side limits. A configured
  `oxa.cloud.subscription_limits` is advisory: it warns at 80% and 100% and is recorded, it is
  not a local refusal (#1443/#1444).
- Keyed Go: a Go key in the owner's own secrets file enables the Go subscription transport for
  the owner's own model choice, with `oxa.cloud.opencode.enabled: false` as the opt-out. Engine
  transport selection stays user-only: no project setting, no delegated `oxa work` model and no
  model restored by `--continue` enables it, and a local alias owns a request only on an exact
  match.
- Go/OpenRouter parsing should tolerate compatible optional fields and stream variations,
  while validating numeric type, sign, range and finiteness before accounting or execution.
- v0.2.1 has no Python in the product/runtime path; build/test tooling is separate.
- Never expose secrets to fork PRs or execute PR code with review credentials. Never use
  `pull_request_target`; unknown authors need `ok-to-review` before token-spending review.
- Go review quota and same-diff checks read at most 200 GitHub comment pages (20,000 comments).
  If either history exceeds that bound, the review defers before reserving quota or requesting a
  provider token; an incomplete history must never authorize another review.
- **Intent:** the change must do what its linked issue and PR description say, and nothing else.
  Flag as P1: missing or contradicted acceptance criteria from the linked issue; unrelated changes
  mixed in; a default that contradicts the owner decision stated in the linked issue or PR body
  (on-by-default with an opt-out setting is normal and is not itself a finding); deleted or
  disabled existing behaviour, tests or checks without a `Removes: <what> — <why>` line in the PR
  body; a test weakened, skipped or rewritten to pass instead of fixing the code.
- **Security policy changes carry their rules:** a change to approval, sandbox, credential, secret or
  publish behaviour updates, in the same PR, the review guidelines here, `SECURITY.md`, and the user
  docs (`oxa/docs/pages/`) that describe it. Flag a missing update as P2. A change to an ordinary
  default, limit or budget updates the user docs; a missing docs update is P3 and is fixed forward.
- **Early-stage process (owner decision, until v0.3):**
  - Only P0/P1 findings, security findings and an open security review block a merge. P2/P3 findings are filed as follow-ups and fixed forward on main.
  - Prefer one PR per feature, up to about 800 changed lines, over stacks of small PRs.
  - A merge that breaks required checks on main is reverted at once, then fixed forward. A review finding
  that contradicts an owner decision stated in the linked issue or PR is answered by updating these
  rules in that PR, not by changing the behaviour.
- **Bash and subagent limits.** The two caps are user-only. `oxa.bash.maxJobs` defaults to 16
  running background jobs.
  `oxa.subagent.maxChildren` defaults to 8 running or queued children per parent and 8 steps
  per call. Both settings have a minimum of 1. An explicit bash `timeout: 0` disables the
  timeout for that call, including a background job, and overrides the foreground default.
- **Cargo cache mounts.** The sandbox forwards an owner-set `CARGO_TARGET_DIR` as a
  toolchain location. An external cache needs a separate user-only
  `oxa.sandbox.writePaths` entry. Review environment forwarding and writable mounts independently.
- **Commands:** a new CLI subcommand, slash command or user-facing flag must be declared in the command
  registry with its surfaces and help (P1 if added outside it or without an exception reason).
- **Worker mode.** An `oxa work` worker is started with `--yolo` only when the owner's user-layer
  `permissions.defaultMode` is `yolo`; otherwise it keeps the sandboxed default. Project settings
  cannot raise it. See `oxa/docs/work.md`.
- **Work acceptance.** In `oxa work`, zero or skipped checks leave the task `Unverified`. The
  owner-only `oxa.work.localChecks` defaults to a per-package `cargo check`. A verified Rust
  acceptance receipt requires hosted test evidence at the exact head, including for changed
  inputs outside packages in a tree with Cargo manifests. Worker declarations
  can add hosted requirements. Review their coverage against the task's acceptance criteria.
- Owner policy: nothing blocks by default except the security invariants above (secrets,
  sandbox, user-only settings, offline, fork safety). Limits, budgets, retries and guards are owner
  settings with generous defaults that warn and record; a hard block exists only when the owner
  configures it. Do not ask for a new default block or for a feature to be default-off.
  The repeat guard follows this: the third identical tool result gets a short note, a call that
  still repeats gets one warning (shown and kept in the session) and the run continues; `oxa.maxTurns`
  is the cap against loops the guard cannot see.
  Report dedupe follows it as well: `oxa.reportIssue.semanticDedupe` is the owner's consent for the
  search and the model check, the post asks one question, and `oxa.reportIssue.dailyNew` (unset = no
  check) only warns and records over the cap. A finding that asks for a second consent question or
  for the cap to block is answered here.
- Reserve P0/P1 for violations of these invariants or concrete data loss/crash. Report only
  introduced, actionable defects with a failure scenario; skip style and speculative risks.
- A finding is a claim to verify, not an order. Whoever acts on a review finding first shows it is
  real (a failing test, a concrete input or the exact code path) and fixes it with that test. If it
  is not real, change nothing for it and answer on the thread with the evidence. Never widen
  scope, refactor or rename to satisfy a reviewer.
- Reuse before you write: search for an existing helper (settings and env parsing, trusted
  executable lookup, private/atomic file writes, subprocess limits, GitHub calls, retry, redaction,
  test fixtures) and call it. A second implementation of the same responsibility is a P2 review
  finding; a weaker one (looser permissions, symlinks, limits) is P1.
- Issues filed by agents state how the problem was observed (command, run or log line). Verify an
  agent-filed issue against current main before building on it; close it with the evidence if it
  does not reproduce.

## How to work

- **One PR per task**, base `main`, branch `agent/<task-id>-<slug>`. Small PRs; resolve real text conflicts
  with `main` using normal merge commits, without rebasing just to update the base. `main` is protected:
  cleared PRs enter the merge queue, which validates the integrated change before merging.
- **PR description:** what and why (cite the task id), what you removed or changed, the tests you ran and
  the CI result. Label the review tier:
  - **T1:** test tooling, one-off scripts, docs.
  - **T2:** product code.
  - **T3:** the sandbox, data deletion, publishing, secret or key handling, CI/workflow security.

  Expect one review round for T2 and more for T3.
- **CI is the judge:** `.github/workflows/checks.yml` on GitHub-hosted runners (x86_64 + aarch64). If a
  workflow does not start on your PR (fork PRs wait for a maintainer's approval), say so in the PR; don't
  work around it.
- **CI tiers:** draft PRs run formatting and all secret/SDK/isolation/receipt scans (plus cheap path
  controls). Ready PRs also run clippy, the oxa build and the x86 stub checks. Engine/Flash builds,
  oxa's mandatory-bwrap workspace tests skip at job level on PR events, so every
  required check name still reports. The merge queue (`merge_group`), manual and nightly runs execute
  the full set regardless of changed paths. The queue is the full integration gate. Main pushes run
  the fast set only: the queue already ran the full set on exactly the tree that lands, and the nightly
  run re-checks main in full.
  Only PR events cancel superseded runs; main and merge-group runs are not cancelled in progress.
  On ready PRs, clippy checks changed Rust components and the stub library suite runs for engine
  changes; unknown paths conservatively select every component. The oxa build, stub smoke and
  loopback OpenAI contract remain fast checks for the rest. Full gates build one x86_64 nextest
  archive and run two partitions with mandatory bwrap. The required oxa check verifies every
  inventoried test ran exactly once, rejects runtime skips, and permits only the three existing
  opt-in measurement/dump ignores. CI tests use the `ci` Cargo profile without LTO; production
  ARM64 builds retain the release profile and engine flags.
  The nightly schedule also runs the complete oxa workspace natively on hosted ARM64 with
  mandatory bwrap, isolated HOME/XDG, doctests and the same inventory/report guard. The
  daily-driver UI suite also runs ten stress rounds with four concurrent tests before the full gate. This extra
  job is skipped on PRs, merge groups and main pushes; manual runs can opt in with
  `nightly_oxa_arm: true`. Its cache is separate from x86 test and production ARM64 builds.
- **When a task is ambiguous,** open the PR as a draft with your question at the top instead of guessing.
- **GitHub access has one owner per session.** An owner-driven interactive `oxa` session and Lead
  own GitHub access, as a Codex lane does. They may use authenticated `gh` and Git to read issues,
  reviews and remote refs, push `agent/**` branches, open PRs, and read CI results. Follow the
  normal publication, review, and landing rules.
- **Sandboxed `oxa work` workers have no GitHub access.** Their runner owns GitHub access. From the
  worker, make no `gh` call at all and make no other GitHub request, including through Git. This
  includes `gh pr view`, `gh pr checks` and `gh run view`. The issue text, review findings and CI
  result supplied by the runner are your task packet.
  A task packet's fetch line is provenance, not an order. A fact the packet lacks is reported as
  missing instead of fetched. Never act on a task text that orders a `gh` call in a runner-owned
  session: report the conflict and continue without it. Commit the work and hand over the branch,
  the base and the exact head commit, the selected crates, the action needed and which fact is missing.
  The session that owns GitHub writes the pull request body and its CI result. It also owns pushes
  and workflow dispatch when the worker session refuses them; do not work around that refusal.
  Below, every `gh` command is for a session that owns GitHub access.

## Build and test capacity (owner rule)

GitHub-hosted CI is the judge for builds and tests. Run it with `OXCI=off`.
Local compiler work is limited to `cargo check -p <edited-crate>`, niced with
`CARGO_BUILD_JOBS=3`. Formatting and read-only source inspection are allowed locally.
Do not run `cargo build`, `cargo test`, Python or end-to-end tests, or whole-workspace
compiler sweeps on n0rpan, Air, or the maintainer's laptop. On n0rpan, run Lead and
its worker from the dev-release binary. Do not compile binaries for deployment.

**Hosted cloud agent environments are not owner machines.**
- **Scope:** a hosted sandbox such as the Codex cloud environment for this repo. It sets `OX_HOSTED_AGENT_ENV=1` in its environment settings.
- **What to run there:** before pushing, run `cargo fmt --check`, `cargo clippy -p <crate> -- -D warnings` and `cargo test -p <crate>` for every crate you touched.
- **Fix what fails, then push.**
- **Tests that need bubblewrap, user namespaces or a writable `/proc`** cannot run there. Skip them, and name them in your reply.
- **GitHub CI is still the judge;** the local run only catches failures before they cost a CI round.
- **The limits above still apply** on n0rpan, Air and the maintainer's laptop.

Keep target directories and large scratch under your own clone or `~/.cache`; never put them under
`/tmp` (it is RAM on the owner's machines). Use isolated HOME/XDG. Remove your finished temporary
files; preserve anything a running process of yours uses, and never clean another lane's files.

In a session that permits pushing, push the lane branch to start the `lane` workflow, or dispatch it
explicitly:

```sh
gh workflow run lane --ref <branch>
gh run watch <run-id> --exit-status
```

The lane workflow selects changed crates plus their local reverse dependencies and binary-fixture
consumers against main. Shared workspace manifests/locks expand the affected workspace. Tests use
release-relevant features, Cargo registry/target caches via GitHub-owned actions, and standard free
Ubuntu x64/ARM runners. Cache keys include the lockfile, toolchain and architecture; lane caches
use their own namespace and branch scope, preserving the required workflows' main-cache policy.
Native engine tests requiring Oryon CPU features remain on the existing self-hosted route. The lane
workflow does not replace or change the merge queue's required checks. Start full-workspace and
release CI only when the change is close to done. Each started job records its queue delay in its
GitHub job summary so capacity decisions use measurements.

When you may call `gh`, wait with **one blocking `gh run watch --exit-status`**, or continue an
independent piece and read the result once later; when the runner owns GitHub, it reads the result.
Never spend repeated model turns or API/status polls waiting for CI, and never
post “still waiting” updates. On failure, read only the failed job's tail:

```sh
gh run view <run-id> --log-failed | tail -100
```

Fix the failure, then test the changed head. Do not repeatedly run unchanged checks or substitute
an earlier head's result. Report the selected crates, exact head and terminal CI result in the PR.

## Definition of done

Run these in GitHub CI before saying a task is done. Automatic local helpers must dispatch CI and consume its result rather than launch builds or tests.

- **oxa** (Rust, in CI from `oxa/`):
  - `cargo fmt --check`
  - `cargo clippy -p <crate> -- -D warnings` for each touched crate
  - `cargo test -p <crate>` for each touched crate
  - UI/TUI change: CI must build the real binary and drive it through the existing PTY/tmux live checks. Unit tests alone are not enough; they have passed for changes that did nothing in the real TUI.
- **Bug fix:** show the new test failing before the fix.

## Hard rules (never break these)

1. **Never weaken a security check or scanner:** the secret / SDK-material / product-isolation scans in
   `checks.yml`, the receipt scrubber, allow-lists, the oxa sandbox, permissions and secret handling. Don't
   split or obfuscate strings to get past a scanner. A test fixture that must look like a secret gets a
   narrow, exact-path exclusion with a written reason, reviewed as T3.
   The user-selected yolo mode is the single deliberate exception, where the user has switched the permission floor off for the session.
2. **No secrets, keys, tokens, transcripts, personal data, real home paths, host names or e-mail
   addresses** in code, tests, fixtures, docs or commit messages. Use placeholder paths (`~/project`), `example.invalid` and
   synthetic data. Commit with a GitHub no-reply e-mail address.
3. **No Qualcomm SDK material** (Hexagon SDK / QAIRT headers, IDL, stubs, skeletons, DSP binaries, copied
   text or constants) and no binaries at all. NPU device code is not developed in this repository; only
   `engine/npu/oxn/common/` (shared CPU-side quantisation headers) lives here.
4. **Workflows run on GitHub-hosted runners only.** Never add `self-hosted` or custom runner labels, never
   use `pull_request_target` or `workflow_run` with PR code, keep `permissions: contents: read` as the
   default, pin every action by full commit SHA, and add no secrets (the release workflow uses only the
   job token and the protected `release` environment).
5. **Don't change engine numerics, default flags, thresholds or gate limits,** and don't edit benchmark
   *results* (`oxc/benchmarks/results/`): they are receipts of what was measured.
6. **Don't hand-rename** `oxc/` → `oxc/` or the `oxc` / `oxc` / `OXC_*` names. The
   rename lands as one dedicated, script-driven PR by the maintainer; write names as they are on `main`.
7. **No tags, releases or publishing.** Releases are cut by the maintainer.
8. **Tests must test real behaviour** (no test theatre). A deleted feature's tests are deleted with it. A
   changed behaviour gets a test that fails before your change and passes after.
9. **Cloud models in tests:** mock servers only. Tests and CI never read a real API key and never call a
   real cloud API (isolated `HOME`, environment cleared).

## Wave A: independent, start now

| ID | Task | Tier | Acceptance |
|---|---|---|---|
| A3 | Engine env knobs with no reader: remove `OXE_PF_PFG_SPLIT`, `OXE_SPIN_WAIT` and `OXE_UCLAMP_MIN` from `engine/crates/server/src/startup_check.rs` and their tests in `tests_v020_round1.rs`. Move test/bench-only names out of the serving binary's table into a test-only list if that's simple. (Ported from the pre-public work: #3.) | T2 | `startup_check` accepts no knob that nothing reads; checks green. `uclamp` comes back later with its reader. |

(A1 and A5 are done; A2 and A4 concerned the maintainer's private CI and are not part of this repository.)

## Wave B: remove the `pi` profile from oxa (open)

Do the steps in this order; each is one PR on top of the previous one.

**Keep:** MCP; the sandbox, permissions, trust and `--offline`; the extension API and QuickJS, and **all
hooks**; RPC mode (the lab tooling uses `prompt`, `get_state`, `abort`, `--continue` and the
`tool_execution_*` events); JSON mode; licence and attribution files and the "ported from pi" comments; the
generic provider/engine layer (oxa may later talk to cloud OpenAI-compatible endpoints through oxc; don't
hardwire it further to the local engine).

| ID | Task | Tier |
|---|---|---|
| B1 | Make `local` the default (still selectable) and run the **whole** oxa workspace under it. Only the parity crate reads `OXA_PARITY_PROFILE`; the other tests follow the default, so this step exposes every hidden pi assumption. Fix them. (Ported from the pre-public work: #9.) | T2 |
| B2 | After this there is NO profile name at all: no `--profile`, no `oxa.profile`, no `OXA_PROFILE`; docs say "oxa's tools/context handling", no brand name. Delete the `Profile` type and let the compiler find every branch: `oxa-proto/src/profile.rs`, `oxa-core/src/profile.rs`, the `Profile::` branches in `oxa-core/src/tune.rs`, `compaction.rs`, `prefix.rs`, `session.rs`, the `oxa.profile` setting and its env override in `oxa-config`. The local values become the defaults. Keep the thinking policy in `oxa-config/src/profile.rs`. In the same PR, remove any pi pass and `OXA_PARITY_PROFILE`/`profile_env` from the workflows. | T2 |
| B3 | Remove pi session import (`oxa-session/src/migrate.rs`, `/import`, rows SLASH-016, SES-008 and FMT-019, `h_import_session.rs`, the import part of `b14_fork_clone_import_picker_metadata.rs`) and `oxa config --import-pi` (`oxa-cli/src/config_cmd.rs`, `args.rs`, rows CLI-053 and CFG-008). | T2 |
| B4 | Tests: drop the "O" rows in `PARITY.md` (they assert that a pi feature is absent) and the pi-exact branches in `oxa-parity/tests/{tools,compaction,extensions}.rs` and in `oxa-core` tests naming `Profile::Pi`. Rename `fixtures/pi` to plain goldens. Rewrite the ship-gate text: the parity suite becomes oxa's own behaviour regression suite. | T1/T2 |
| B5 | Settings and docs: remove the pi fallbacks (`defaultTools` restoring pi's four, `thinkingBudgets` accepted with a warning), replace `oxa/docs/differences.md` + `oxa/ci/gen_docs.py` with one paragraph "derived from pi", retire `WAVES.toml`, and remove the I3 matrix code and any pi job in `.github/workflows/`. | T1/T2 |

## Wave C: after the maintainer says "Wave C go"

| ID | Task | Tier |
|---|---|---|
| C4 | Engine features that are default-off with no user path: fair time-slicing (`fair.rs`, `--fair-quantum-ms`) and the MTP guard (`mtp_guard.rs`). **Don't remove them yet**: open a draft PR that adds an oxc config switch for each (default off), so they can be measured on hardware. | T2 |

## Wave D: cloud models through oxc (implement now; D1 and D4 are T3)

oxc is the gateway; oxa keeps a single provider type (oxc). Every client gets cloud models through the same
OpenAI v1 endpoint by choosing a model name.

- **D1 upstream routing:** a model catalog entry kind `remote` (OpenRouter, OpenAI-compatible: base URL,
  upstream model id, context window, max output, pricing). Requests for a remote model are proxied upstream
  with `provider: {zdr: true, data_collection: "deny"}` always set. The API key lives once in an oxc secrets
  file (0600, outside the repo), read at request time, NEVER logged (redact it in errors and debug output),
  never in status output.
- **D2 privacy switch:** cloud is OFF by default (`cloud.enabled: false`); remote models are not even listed
  while off. oxc's offline mode hard-refuses remote. Every remote request is logged in the usage ledger with
  `origin` and `remote: true`; a per-day budget cap (USD) refuses when exceeded.
- **D3 normalisation:** stream pass-through with oxc's existing OpenAI v1 normalisation (tool-call streaming,
  usage, reasoning fields, `reasoning_effort` mapping); conformance tests reuse the contract suite against a
  mock upstream.
- **D4 capabilities:** `/_local/status` and `/v1/models` expose per-model capabilities (`remote`, `echo`,
  `cache_hints`, `cancel`, `prefill`, `context_window`). oxa reads them per model, skips local-only features
  for remote models, takes the context size from them, and refuses a remote model under
  `--offline`/`OXA_OFFLINE`.
- **D4b status bar:** the footer segment shows where the model runs: `local` (theme green) or
  `cloud · openrouter` / `cloud · anthropic` (a distinct theme colour), from oxc's per-model `remote`
  capability; also in the start banner. Test both states.
- **D5 tests:** a mock OpenRouter server (no network): routing, the key never logged or exposed, cloud off by
  default, offline refusal (oxc and oxa), the budget cap, an oxa tool-call loop through oxc to the mock.
- **D6 Anthropic upstream:** catalog entries of kind `remote/anthropic`, implemented in oxc in Rust against the native Messages API (not an OpenAI-compat shim): translate OpenAI v1 ↔
  Messages (tools, streaming tool calls, `stop_reason` incl. `refusal`), map thinking to adaptive thinking +
  `output_config.effort`, and place `cache_control` breakpoints on the frozen prefix and the latest history
  turn. Same key handling, cloud-off default, budget cap and ledger as D1/D2. Tests against a mock server only.
- **D7 "Sign in with ChatGPT" (design only):** a short design for oxc as a participating app (OAuth flow,
  per-app limit display, where tokens live, cloud-off default). By owner decision (issue #252) the
  experimental Codex sign-in route ships its public client identifier, the way other open-source harnesses
  do. The official program replaces it once it is generally available (#247).

## Done means

All Wave A PRs merged; Wave B merged in order; Wave C/D PRs opened. Leave a short summary comment on the last
PR: what merged, what's waiting and why.
