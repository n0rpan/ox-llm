# Security

## Reporting
Please report vulnerabilities privately through GitHub Security Advisories ("Report a vulnerability" on this repository). Do not open public issues for security problems.

## Review guidelines (used by automated security review)
Read `docs/threat-model.md` first. Report a finding only if this change weakens one of its trust boundaries; explain the trigger and the impact, and cite the changed lines. The sandbox standard and the owner decisions below are part of the standard, and so are the review guidelines in `AGENTS.md`: a finding that asks for the opposite is not a finding.

Treat as **high** severity:
- a secret (API key, ChatGPT token, OpenCode/GitHub token, share key) printed, logged, stored outside its 0600 store, sent to the wrong host, or reachable by model tools;
- a project file or model output able to change user-only settings, enable a feature flag, or bypass an in-process guard, the network guard, or the OS sandbox of an unattended run without yolo;
- metered API requests that bypass required reservation or settlement, are missing from the ledger, or bypass a **configured** daily cap;
- OpenCode Go requests that bypass subscription token accounting, or provider-reported paid charges that are missing from the ledger or shared daily spend;
- a project setting, a delegated model or a restored session model that flips engine transport selection;
- fork-PR or guest access to secrets, cloud providers or owner credentials;
- new `unsafe`/FFI/process/network code in the engine without bounds checks;
- a security check deleted silently. An intentional removal is stated in the PR body with a `Removes:` line.

Treat as **medium**: missing bounds on untrusted input (sizes, lengths, recursion), error messages that echo provider text, weakened validation of provider usage/billing fields.

Do not report: style, pre-existing issues outside the diff, test-only code that never runs in production, or hypothetical risks without a concrete path.

## Sandbox standard (owner decision)

- **Interactive CLI:** no OS sandbox by default, on Linux and macOS. Dangerous commands ask the owner.
- **Yolo:** no OS sandbox for the session, nor for any worker, subagent, headless or unattended run it starts. Yolo refuses only publishing secrets and an OS-breaking wipe.
- **Unattended runs without yolo:** keep bwrap by default.
- **In-process guards in every mode:** secret masking, private-path denials, user-only settings, offline mode.
- **Re-adding an OS sandbox to interactive or yolo paths** is a regression, not a hardening.

## Owner decisions (not findings)

Limits are owner settings that warn and record. A finding that asks for a default block, a built-in cap or a second consent question is answered here. These decisions bind code review too; `AGENTS.md` points here instead of repeating them.

- **Off Linux** there is no OS sandbox. In-process guards stay, bash gets the same cleared base environment, a refused bash network disables it, and a run that keeps the network says so at start and to the model.
- **No configured cap, no cap check,** for the owner and for authenticated allowlisted share guests. Guest authority, tokens and request quotas still apply.
- **OpenCode Go** admission reserves subscription tokens only, including when the provider falls back to its paid balance. `oxa.cloud.daily_budget_usd` applies to metered API admission, not to Go admission. Provider-reported Go charges are recorded in the ledger and included in the shared daily spend. A configured `oxa.cloud.subscription_limits` is advisory: it warns at 80% and 100% and is recorded, and it is never a local refusal.
- **Keyed Go:** a Go key in the owner's own secrets file enables the Go subscription transport for the owner's own model choice, with `oxa.cloud.opencode.enabled: false` as the opt-out. Transport selection stays user-only: no project setting, no delegated `oxa work` model and no model restored by `--continue` enables it, and a local alias owns a request only on an exact match.
- **Cargo cache:** the sandbox can forward the owner's `CARGO_TARGET_DIR` to worker shells as a toolchain location. External cache paths are writable only through an explicit user-only `oxa.sandbox.writePaths` mount. Review environment forwarding and writable mounts independently.
- **Headless inbox:** persisted headless print and JSON runs with a pursuing goal consume plain `oxa send` inbox messages with `oxa.inbox.headless` set to `true` by default. This user-only setting can opt out; project settings cannot enable or disable it. Non-persisted sessions, sessions without a pursuing goal, and the managed-steering path do not mount the plain inbox watcher.
- **Bash and subagent limits:** `oxa.bash.maxJobs` (default 16 running background jobs) and `oxa.subagent.maxChildren` (default 8 running or queued children per parent, and 8 steps per call) are read from the user file and have a minimum of 1. Project settings for remote concurrency remain bounded by the user's child cap. An explicit bash `timeout: 0` disables the timeout for that call, including a background job. Sandbox and permission checks still apply.
- **Automated Go review** reads at most 200 GitHub comment pages (20,000 comments) when it checks the configured daily quota and prior same-diff receipts. If either listing is incomplete, review is deferred before quota reservation, provider authentication, or provider I/O. An incomplete history cannot authorize another token-spending review.
- **`oxa work` acceptance:** a task with zero or skipped checks remains `Unverified` and lacks a publishable acceptance receipt. Project files and worker replies cannot change the owner-only `oxa.work.localChecks` setting, which defaults to per-package `cargo check`. Rust acceptance requires hosted test evidence for the exact head, including for changed inputs outside packages in a tree with Cargo manifests. A worker declaration can add requirements to that evidence; review their coverage against the task's acceptance criteria.
- **Repeat guard** (#1448): the third identical tool result gets a notice, a call that still repeats gets one persisted warning, and the run is not stopped. `oxa.maxTurns` is the configured cap against loops the guard cannot see.
- **Board evidence limits** degrade only the affected pull request. A connection that cannot be read completely produces a visible, ineligible row and cannot trigger review, landing, or refill actions; the other rows remain usable, and the limit is logged. An over-page merge queue retains its observed entries, while every unobserved pull request has unknown queue state and cannot trigger a queue action.
- **Report dedupe is one consent** (#1448): opting into `oxa.reportIssue.semanticDedupe` is the owner's consent to send the scrubbed report, a public source excerpt and candidate summaries to the configured model and to search the repository for matching issues, so the post asks one question: the final preview. New issues are limited to one per task. The per-worker daily count is the owner setting `oxa.reportIssue.dailyNew`: unset means no check, and at or over a configured cap the preview warns, the ledger records it and the owner's yes still posts.
