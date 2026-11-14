# oxa-ext

JavaScript extensions (embedded QuickJS, pi's API shape: `export default function (oxa) {}`)
and exec hooks (`settings.json` `hooks`). Both are `dyn oxa_proto::Hooks` to `oxa-core`.

| Module | Task | Provides |
|---|---|---|
| `state.rs` | architect | `ExtState`, `Registry`, `Registered*`, `Handler`, `ExtId`, `LoadedExtension`, `ExtError`, `upcall::*` op names |
| `manifest.rs`, `oxa.d.ts` | E1 | `ExtManifest`, `read_manifest`, `merge_exported_manifest`, `OXA_D_TS` |
| `loader.rs` | E1 | `load_extensions`, `load_source`, `unload_all`, `install_api` (implemented), `ExtModuleLoader`, `LoadOpts` |
| `bus.rs` | E2 | `ExtensionHost` (`impl Hooks`), `reply_from_js`, `serve_upcall` (implemented), `SharedUi` |
| `guard.rs` | E2 | `guard::check`, `guard::accepts`, `GuardVerdict` |
| `api_register.rs` | E3 | `install`, `validate_tool_schema`, `tool_spec_from_def`, `ExtTool` (`impl Tool`) |
| `api_ctx.rs` | E4 | `install`, `serve` (dialogs) |
| `exec_hooks.rs` | E5 | `ExecHooks` (`impl Hooks`), `parse_exec_hooks`, `hook_stdin`, `reply_from_exit` |
| `api_ui.rs` | F1 | `install`, `serve` (widgets), `complete` |
| `api_components.rs`, `js/tui.js` | F2 | `install`, `serve` (custom), `JsComponent`, `TUI_JS` |
| `api_render.rs` | F3 | `install`, `render` |

## Cross-task contracts

- **Shared state** is `ExtState` (`state.rs`): every `api_*::install(rt, st, ext)` registers
  into `st.registry`; the bus reads it. Never hold a registry lock across a script call.
- **Threads**: all extension runtimes live on one `oxa_script::RuntimeThread`; the session
  thread calls `thread.run(job, serve)`. A host function that needs the user calls
  `CallCx::upcall({"op": ..})`; `bus::serve_upcall` answers it with the `&mut dyn Ui` that
  `Hooks::fire` / `run_command` received. Op names and payloads are fixed in `state::upcall`.
- **F-wave functions are safe no-ops in W0** (`api_ui`, `api_components`, `api_render`), so
  wave E loads and runs extensions before wave F lands.
- **W-guard** (`guard::check`, pure): observer replies ignored; append-safe points accept only
  the replies in the table on `oxa_proto::HookReply`; prefix-checked points accept a list only
  if the current wire is a byte prefix of it, unless the extension declared `rebase: true`
  (then `DeclaredRebase`, logged as a miss cause). The core tells the bus the current wire in
  the payload field `messages` of `context`, `context_with_system`, `before_provider_request`.
- **Host access**: `oxa.exec`, `ctx.fs`, `ctx.fetch`, session actions all go through
  `Host::call(HostAction)` (`oxa-core/src/host.rs`); the policy is applied there.
- **Tools from extensions** are `ExtTool`; class defaults to `Exec` (oxa addition: `class` in
  the definition). `Tool::run` has no `Ui`: dialogs from a tool use `ExtensionHost::set_tool_ui`
  when the front end provided one, else answer `null`.
- **Errors** from extension code never propagate: they go to `ExtState::errors`
  (`take_errors`, emitted by the core as `extension_error`) and count as `Continue`.
- **Omitted**: `registerProvider`, `registerVirtualModel`, `registerMcpServer` /
  `unregisterMcpServer` / `getMcpServers`, `ctx.newSession()` / `ctx.switchSession()`,
  `before_provider_headers`, Node/npm imports -> `unsupported_in_oxa`. `.ts` files are rejected with a hint (see `oxa-script`).

## Acceptance

- E1: discovery dirs and order; sync/async factory; load error isolates one extension;
  `/reload` drops contexts; `oxa.d.ts`.
- E2: handler order and composition as extensions.md; W-guard: non-prefix `context` result
  refused, declared rebase logged.
- E3: registerTool (schema validation), Command, Shortcut, Flag; sendMessage / sendUserMessage
  delivery modes; appendEntry; active tools.
- E4: ui dialogs; session read API; compact; context usage;
  fork/navigateTree/reload/waitForIdle (newSession/switchSession -> `unsupported_in_oxa`);
  exec + fs + fetch through
  `Policy`.
- E5: JSON in/out contract; runs in bwrap; timeout; a failing hook blocks only where documented.
- F1: setStatus/Widget/Footer/Header/Title; autocomplete providers; working message.
- F2: JS component `render(width)` + `handleInput`; overlays; custom editor; the pi-tui shim.
- F3: message, entry, tool renderers; Markdown transformers with `messageTypes`.

## Used by

`oxa-cli` (wires `ExtensionHost` and `ExecHooks` into `Deps::hooks`), `examples/extensions`.

## Post-wave-E (lane L4)
- Dialogs (`select`, `confirm`, `input`, `editor`, `custom`) stop the handler's deadline clock
  while open (`CallCx::upcall_paused`); `notify` and the setters do not.
- Event handlers and extension tools run "in the run scope": their `ctx.waitForIdle()` is refused
  with `busy` while the session is not idle (it could never resolve); commands and shortcuts may
  wait.
