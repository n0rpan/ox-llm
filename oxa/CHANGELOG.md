# Changelog

## [0.2.0] - 2026-11-13

### Added

- First public release of oxa: a Rust reimplementation of the pi 1.5.2 coding agent for the oxe engine.

### Engine-facing behaviour (for anyone running oxa against a real engine)

- The boot preload (the zero-token prefill of the frozen prefix that oxa sends when a session opens and nothing has been typed yet) now carries `X-Oxe-Snapshot: keep` whenever the engine advertises checkpoints, exactly like the idle prefill and the park. Before, it never sent the hint. It takes one kept-checkpoint slot of the session at the prefix boundary.
- A `subagent` child process now dies with its parent: the child is started with the Linux parent-death signal (SIGTERM), so a killed or crashed parent no longer leaves a child holding an engine request. Abort still ends the child's process group (TERM, then KILL after 2 s).
- A cache miss is never reported with the cause `unknown`: without a rebase it is `cold_start` on the first request and `engine_evicted` afterwards.
- `rust-version` is 1.87; the release profile unwinds on panic.
- Repeated reads: a read-only call that already returned the same content three times in a row (no edit, write or command in between, same prompt) gets a short note instead of the content; after two notes the next repeat stops the run (`oxa.toolRepeatGuard`, on by default). See [tools.md](docs/pages/tools.md#repeated-reads).
- `oxa.sampling.mode: model` (opt-in) sends the model card's sampling for Qwen3.6 and Ornith instead of `temperature: 0`. The default stays greedy: the engine's speculative decoding runs on greedy requests only.

### Not in oxa (removed before the release)

- `npm:` package sources and `npmCommand` (git URLs and local paths stay).
- `newSession` / `switchSession` and `registerMcpServer` from extensions (they answer `unsupported_in_oxa`).
- The tmux modifyOtherKeys fallback, OSC 9;4 progress and OSC 7501 program status, the terminal capability overrides, the SSH ESC-timeout rule, the hardware-cursor option, the changelog display and `/changelog`, `codemode.inlineBudget`.
