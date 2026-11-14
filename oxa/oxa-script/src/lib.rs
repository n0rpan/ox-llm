//! W0-FROZEN
//! # oxa-script: embedded JavaScript (QuickJS) for extensions and codemode
//!
//! The only crate that names `rquickjs`. `oxa-ext` and `oxa-codemode` use this API and exchange
//! plain [`serde_json::Value`]s with scripts.
//!
//! ## Contracts
//!
//! * **No ambient authority.** A fresh [`Runtime`] has the ECMAScript built-ins, `console`
//!   (routed to [`Runtime::set_console`]) and nothing else: no fs, no network, no timers, no
//!   process, no `import` beyond the embedder's [`ModuleLoader`]. Every capability is a host
//!   function registered by the embedder, and the embedder passes it through `Policy`.
//! * **Limits.** [`Limits::memory_bytes`] is a hard heap cap; [`Limits::deadline_ms`] bounds one
//!   outermost call into the script (eval, `call`, `call_export`), enforced through QuickJS's
//!   interrupt handler. Hitting either yields `Err(code = "script")` and leaves the runtime
//!   usable for the next call (out of memory: the context is dropped and must be recreated).
//!   [`InterruptHandle::interrupt`] (any thread) stops the running call the same way.
//! * **Value bridge.** JS -> Rust: JSON-compatible values map 1:1; `undefined` and absent map to
//!   `Value::Null`; a function maps to `{"$fn": <id>}` (see [`FnRef`]), also when nested in
//!   objects and arrays; a `Uint8Array` maps to `{"$bytes": "<base64>"}`; an `Error` maps to
//!   `{"$error": {name, message, stack}}`. Rust -> JS: JSON values 1:1; `{"$bytes": ..}` becomes
//!   a `Uint8Array`; `{"$fn": id}` becomes the original function.
//! * **Promises.** A call that returns a promise is awaited by pumping the job queue. A promise
//!   that cannot settle (job queue empty, nothing pending) fails at once with `script`
//!   ("promise can never settle"): there are no timers.
//! * **Threads.** A [`Runtime`] is `!Send` and lives on one thread. [`RuntimeThread`] owns such
//!   a thread and is `Send + Sync`; that is what `Hooks` implementations hold.
//! * **Reload.** Dropping a [`Runtime`] (or [`Slots::remove`]) frees the context and
//!   invalidates every [`FnRef`] and [`ModuleRef`] made by it; using a stale one is
//!   `Err(script)`, never UB or a panic.
//! * Errors thrown by scripts become `OxaError { code: "script", message: "<name>: <message>",
//!   hint: Some(<stack>) }`. An `OxaError` returned by a host function is thrown into the script
//!   as an `Error` whose `message` is the error's message and whose `code` property is its code.
//!   Such an error that is not caught (or any thrown error whose `code` property is one of the
//!   [`oxa_proto::codes`]) comes back with that code instead of `script` (wave H).

mod host;
mod module;
mod runtime;

pub use host::*;
pub use module::*;
pub use runtime::*;
