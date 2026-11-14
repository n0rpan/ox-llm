# oxa-script

Embedded JavaScript (QuickJS through `rquickjs`) for extensions (`oxa-ext`) and codemode
(`oxa-codemode`). The only crate that names `rquickjs`.

| Module | Task | Provides |
|---|---|---|
| `runtime.rs` | A15 | `Limits`, `Runtime` (eval, modules, calls, limits), `InterruptHandle`, `ModuleRef` |
| `host.rs` | A15 | `HostFn`, `CallCx`, `FnRef`, `RuntimeThread`, `Slots`, `CallbackPort` |
| `module.rs` | A15 | `ModuleLoader`, `MapLoader`, `join_specifier`, `strip_types` |
| `lib.rs` | architect | contracts (read its crate docs first) |

## Cross-task contracts

- **No ambient authority**: a fresh runtime can compute and nothing else. Capabilities are host
  functions; the embedder checks `Policy` inside them.
- **Value bridge** (crate docs): JSON 1:1, functions as `{"$fn": id}` (`FnRef`), bytes as
  `{"$bytes": base64}`, errors as `{"$error": ..}`.
- **Limits**: memory cap and per-call deadline produce `OxaError { code: "script" }`; the
  process never dies and never hangs because of a script.
- **Threads**: `Runtime` is `!Send`. `RuntimeThread` is the `Send + Sync` owner used by `Hooks`
  implementations. `RuntimeThread::run(job, serve)` blocks; host functions reach the caller's
  thread with `CallCx::upcall` (dialogs), and the caller reaches blocked script functions with
  `CallbackPort::call` (custom components).
- **Promises**: awaited by pumping jobs; a promise that can never settle is an error (no timers).
- **Reload**: dropping a runtime invalidates its `FnRef`s and `ModuleRef`s; stale use is an error.

## Acceptance (A15)

Host-function binding; ES module loader; memory cap and deadline interrupt a runaway script; no
ambient fs/net. Tests: `tests/a15_*.rs`.

## Used by

`oxa-ext` (E1-E4, F1-F3), `oxa-codemode` (E6, F4).
