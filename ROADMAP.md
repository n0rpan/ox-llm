# Roadmap

The next release is **v0.2.0**: the engine, the controller and oxa ship together from this repository,
with one tag and one release page. See [DECISIONS.md](DECISIONS.md) for the reasoning.

## v0.2.0 (in progress)

1. **oxa:** a fully featured Rust rewrite of the pi 1.5.2 coding agent, tuned for this engine. The work
   comes in waves: the workspace and protocol first, then the core, tools, sessions, compaction, TUI,
   MCP and extensions. A parity suite gives every pi behaviour a test or a recorded omission. Final
   check: a hardware A/B against pi and OpenCode on the same tasks.
2. **Client-neutral core:** client quirks come out of the engine and controller (no pins, no client
   templates). Generic robustness stays in: concurrent requests are queued and replays are safe.
   Diagnostics are client-agnostic.
3. **Engine kernels:** the f16 GEMM-shaped prefill attention (`pfg`, opt-in) and position-blocked
   decode `V^T.p` (`pb`, bit-identical) are promoted only if their decision measurements and gates
   pass. User-message checkpoints (`--user-snapshots`) are default-off until their gate passes.
4. **Operations:** a health digest, a release-update check and recovery drills.
5. **Release:** one squashed public snapshot under a new public repository name, which the owner
   chooses at release time. The install gate pack runs once on the final binaries.

## After v0.2.0

- Maintenance mode: no always-on agents. Issues come from use and from the health digest.
- Research is driven by the data, with pre-registered thresholds. Parked: NPU prefill/decode, int8/fp8
  KV, 256K contexts. Rejected paths are kept in [lab/docs/FAILED-PATHS.md](lab/docs/FAILED-PATHS.md).
