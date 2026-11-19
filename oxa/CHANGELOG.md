# Changelog

## [Unreleased]

### Fixed

- `oxa work` derives acceptance requirements from the change (#1442). The owner's `oxa.work.localChecks` defaults to `cargo check` for touched packages. Rust tests require hosted lane CI at the exact head. Workers can declare additional acceptance crates with `<oxa-acceptance>["crate"]</oxa-acceptance>`. Zero or skipped checks leave work `unverified`. Each round records check origin and verdict, and saved verification is bound to the exact commit. See [work.md](docs/work.md).
- `oxa work` starts a new task branch at its published head (`origin/<branch>`) when that tip descends from the captured base, instead of always at the base. A task for an already pushed pull-request branch now audits and continues that head rather than redoing work from a tree that predates it; several diverging tips or an unrelated one keep the base. See [work.md](docs/work.md).
### Added

- An interactive session without a usable `bwrap` now runs bash unsandboxed with a start warning instead of disabling bash, and an unenforceable `oxa.sandbox.net: off` keeps bash with a "network is NOT restricted" warning (offline still disables it). Unattended runs keep "bash is disabled" (#1512). Part of the yolo power plan (S10).

- `oxa work --pr N --model MODEL` (#1019): the unattended packet for a pull request carries its title, body, reviews, issue comments and the names of failing checks at the current head, and the task branch is the pull request's own head branch. Review text and check names are labelled as data and bounded (reviews to 16 KiB, 40 check names); only failing checks travel, never passing ones. Malformed review or check lists are refused, never silently dropped.

### Changed

- Provider preconnect (#881): qualified as not implementable through the existing gateway pool. The pinned reqwest 0.12 transport has no preconnect or pool-insertion API, so warming the selected provider would need a real authenticated request after the first frame; no warm-up was added. The limitation and the owner's options are in [provider-preconnect.md](docs/design/provider-preconnect.md).
- Security-relevant default change (#314): upgrading without a saved mode now selects `auto` instead of the previous prompting default. Select `--standard` or save `permissions.defaultMode: "standard"` to retain prompts. Owner-selected `yolo` deliberately disables the permission floor for file tools (including credentials and git hooks/config) and the bash sandbox; `auto` keeps both and plan remains read-only.
- Permission modes (#202): `auto` is the old YOLO (no prompts, bash sandbox on) and the default on a fresh install; `yolo` now means full access (no prompts, no sandbox); `standard` still prompts. `permissions.defaultMode` (`standard` | `auto` | `yolo` | `plan`) sets the mode at start and replaces `permissions.yolo: "auto"`; a project's settings may lower it but never raise it to `yolo`. `/mode <name> --save` or "make default" in the `/mode` picker writes it, flags override per run, one key cycles standard → auto → yolo and the status bar always shows the mode.

## [0.2.0] - 2026-11-13

### Fixed (first run)

- The bash sandbox maps only the caller's uid, so root-owned system files show as uid 65534 (`nobody`) inside it; ownership checks that insist on "root or the caller" fail only there. Documented in [security.md](docs/pages/security.md#the-bash-sandbox), and the `exec_no_thp` wrapper test now skips (rather than fails) in such a user-namespace environment.
- Started in your home directory and confirmed (or with `oxa.sandbox.allowHomeWrite`), bash now gets the home as its workspace: its folders and files are writable, the home folder itself, dotfiles, dot-folders and `~/bin` stay read-only, and keys stay masked. `/` and a parent of home are never writable, even with the setting; a symlink to home counts as home. The file tools ask before writing shell startup files and the agents' settings when started in home.

### Added

- `oxa -p` now writes progress to stderr as the run goes (text before tool calls, `[tool]` start and end lines) and the final reply to stdout when the run ends (once, even when a `done` check or a steering message starts another turn after a first reply; that earlier reply goes to stderr), all flushed, so a killed run leaves a log. Stdout is unchanged.

- `done` setting: a list of shell commands (for example `cargo fmt --check`) that oxa runs itself, in the sandbox, when a run that edited files is about to finish. A failure is fed back to the model for up to 2 automatic fix rounds, then reported; nothing changes while the list is empty. An entry `in <dir>: <command>` runs in a subfolder of the project (a missing folder or one outside the project is refused as `FAILED (not run)`). `-p` and `--mode json` exit 1 with one `done checks failed` line on standard error when the checks still fail at the end, and `-p` says so when a project's `done` was skipped because the project is not trusted.
- `OXA_TRUST_PROJECT=1` trusts the project for one run like `--approve` does for trust (without the other effects of `-a`), for unattended runs; `-na` still wins.
- First public release of oxa: a Rust reimplementation of the pi 1.5.2 coding agent for the oxe engine.
- Session files have one writer: a second oxa opening a session that is open elsewhere stops with a clear message and `--fork` as the way out. Stale locks cannot occur (the lock dies with its process).
- A compaction without a model summary (`local` profile) now says how many assistant/tool turns it dropped and shows `no summary` in the footer.
- One warning line at start when the server advertises a newer protocol than oxa understands, or lacks a capability oxa expects (read from the existing status/props calls; no new request).

### Engine-facing behaviour (for anyone running oxa against a real engine)

- The boot preload (the zero-token prefill of the frozen prefix that oxa sends when a session opens and nothing has been typed yet) now carries `X-Oxe-Snapshot: keep` whenever the engine advertises checkpoints, exactly like the idle prefill and the park. Before, it never sent the hint. It takes one kept-checkpoint slot of the session at the prefix boundary.
- A `subagent` child process now dies with its parent: the child is started with the Linux parent-death signal (SIGTERM), so a killed or crashed parent no longer leaves a child holding an engine request. Abort still ends the child's process group (TERM, then KILL after 2 s).
- A cache miss is never reported with the cause `unknown`: without a rebase it is `cold_start` on the first request and `engine_evicted` afterwards.
- `rust-version` is 1.87; the release profile unwinds on panic.
- Repeated calls: a tool call (any tool) that already returned the same result three times in a row (same arguments, nothing in between, same prompt) gets a short note instead of the result; after two notes the next repeat stops the run (`oxa.toolRepeatGuard`, on by default). See [tools.md](docs/pages/tools.md#repeated-calls).
- Repeat guard and turn cap (#507): a failed `edit` or `write` no longer ends a run of identical reads, and the new `oxa.maxTurns` (default unset) ends a run after that many model replies with a `turn_limit` error. See [tools.md](docs/pages/tools.md#repeated-calls).
- Adaptive thinking on tool follow-ups (`oxa.thinking.toolFollowups`, default `auto`, under `oxa.thinking.policy: auto`): routine steps stay off; a failed call, a repeat note or `oxa.thinking.escalation.fruitlessCalls` (6) calls without progress turn thinking on for the next replies, until an edit applies or a test passes. The footer shows `thinking: auto→on` and the transcript a note with the reason. `on` and `off` keep the previous fixed choices. See [tools.md](docs/pages/tools.md#thinking-on-tool-follow-ups).
- `oxa.sampling.mode: model` (opt-in) sends the model card's sampling for Qwen3.6 and Ornith instead of `temperature: 0`. The default stays greedy: the engine's speculative decoding runs on greedy requests only.

### Not in oxa (removed before the release)

- `npm:` package sources and `npmCommand` (git URLs and local paths stay).
- `registerMcpServer` from extensions (it answers `unsupported_in_oxa`). `newSession` / `switchSession` work in the TUI and print mode; RPC still answers `unsupported_in_oxa`.
- The tmux modifyOtherKeys fallback, OSC 9;4 progress and OSC 7501 program status, the terminal capability overrides, the SSH ESC-timeout rule, the hardware-cursor option, the changelog display and `/changelog`, `codemode.inlineBudget`.
