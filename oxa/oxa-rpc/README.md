# oxa-rpc

Headless modes: print, JSON event stream, RPC server, Rust RPC client. Depends on proto, core, session, config, export. Used by `oxa-cli` and `oxa-parity`.

| Module | Task | Provides |
|---|---|---|
| `lib.rs` | architect (frozen) | `RpcCx`, `Out`, `Handled`, `handle_run`, `handle_session` |
| `events.rs`, `print.rs` | C9 | record encoders, `JsonSink`, `run_print`, `run_json` |
| `server.rs`, `cmd_run.rs` | C10 | `serve`, `parse_command`, responses, run-control commands |
| `cmd_session.rs` | C11 | session commands |
| `ext_ui.rs`, `client.rs` | F8 | `RpcUi`, `UiRouter`, `RpcClient` |

Contracts: pi's record shapes field for field (fixtures = the examples in pi's json.md / rpc-commands.md / rpc-extension-ui.md); LF-only framing; stdout = records only; oxa additions are additive (`set_mode`, own events, Ask relayed as `confirm`).
