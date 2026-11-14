# oxa-cli

The `oxa` binary. Depends on every product crate; nothing depends on it except `oxa-parity` (through the built binary).

| Module | Task | Provides |
|---|---|---|
| `lib.rs` | architect (frozen) | re-exports, mode and exit-code contract |
| `args.rs` | C12 | `Args`, `parse_args`, `usage_text` |
| `main.rs`, `dispatch.rs`, `config_cmd.rs` | D13 | `run`, `choose_mode`, `Stdio`, `run_config` |
| `mcp_cmd.rs` | E9 | `run_mcp` |
| `pkg_cmd.rs` | E10 | `run_pkg` |
| `hooks.rs` | E-INT, E11 | `build_hooks`, `HookInputs`, `HookSets` |
| `child.rs` | E11 | the subagent child side: `ChildIdentity`, `ChildEngine` |

Contracts: pi's flags and conflicts (cli.md) with `oxa` names; exit codes 0 / 1 / 2; no engine = one line + exit 2; `main` is the only reader of the process environment; no network unless asked. `oxa_cli::run(argv, env, Stdio)` is fully injectable so tests run it in-process.

Subagent children (E11): `builtin:subagent` is built after MCP and before codemode when `oxa.subagent.enabled` is set and the process is not itself a child; the child binary is `std::env::current_exe()`. A child is recognised by `OXA_SESSION_HINT` (`<parent id>.s<n>`) / `OXA_SUBAGENT_DEPTH` in the `Env` given to `run`: its engine is wrapped in `ChildEngine` (every request carries the hint as its session, `background`, and a chat turn becomes turn kind `subagent`; compaction and prefill keep their kind), `OXA_CHILD_SOFT_TOKENS` overrides `oxa.compaction.softTokens` in memory (`Settings::with_override`), and the subagent hook set is not built (depth 1). Malformed values are ignored with a warning (a garbled depth still counts as a child).
