//! W0-FROZEN
//! # oxa-ext: JavaScript extensions and exec hooks
//!
//! pi extensions are TypeScript modules running in pi's process. oxa extensions are JavaScript
//! modules (`export default function (oxa) { ... }`) running in embedded QuickJS
//! (`oxa-script`), with the same API shape, so ports are mechanical. Language-neutral gates use
//! *exec hooks* (`settings.json` `hooks: {event: command}`). `oxa-core` sees both only as
//! `dyn oxa_proto::Hooks`.
//!
//! ## Contracts
//!
//! * **One runtime per extension**, all on one [`oxa_script::RuntimeThread`]. A load error or a
//!   throwing handler isolates that extension: it is reported (`AgentEvent::ExtensionError`
//!   through [`ExtState::take_errors`]) and treated as `HookReply::Continue`. Nothing an
//!   extension does may panic or hang the session thread (deadline: `Limits::EXTENSION`).
//! * **No ambient authority.** `oxa.exec`, `ctx.fs.*` and `ctx.fetch` are host functions that
//!   go through `Host::call(HostAction::{Exec, ReadFile, WriteFile, Fetch})`, i.e. through the
//!   same `Policy` and sandbox as tools. There is no Node API and no npm import.
//! * **W-guard** ([`guard::check`]): observers cannot change anything; append-safe points
//!   accept only their documented replies; `context` / `context_with_system` /
//!   `before_provider_request` may only return a message list that has the current wire as a
//!   byte prefix, unless the extension declared `rebase: true`, and then the rebase is logged
//!   as a cache-miss cause (`miss_cause::EXTENSION`).
//! * **W3.** Tools, commands, prompt sections and flags are read by the core once per epoch.
//!   `registerTool` after load takes effect at the next epoch (`/reload`), never mid-run.
//! * **Handler order** is pi's: extensions in load order, handlers in registration order;
//!   transforming replies chain (the next handler sees the transformed event); a cancelling or
//!   blocking reply stops the chain.
//! * **Omitted (verdict O):** `registerProvider`, `registerVirtualModel`,
//!   `registerMcpServer` / `unregisterMcpServer` / `getMcpServers`, `ctx.newSession()` /
//!   `ctx.switchSession()` (refused by the core host), `before_provider_headers`, Node / npm
//!   imports. Calling them throws an `Error` with `code = "unsupported_in_oxa"`.
//! * `/reload` = [`ExtensionHost::reload`]: every context is dropped, then loaded again.
//!
//! ## Module map
//!
//! Each `api_*.rs` exposes `install(rt, st, ext)` which binds its part of the `oxa.*` / `ctx.*`
//! surface on one extension's runtime, and (where it needs the caller's UI) a `serve` function
//! for upcalls. `loader.rs` calls every `install`; `bus.rs` calls every `serve`.

pub mod api_components;
pub mod api_ctx;
pub mod api_register;
pub mod api_render;
pub mod api_ui;
mod bus;
mod exec_hooks;
pub mod guard;
mod loader;
mod manifest;
mod schema;
mod state;

pub use api_register::ExtTool;
pub use bus::*;
pub use exec_hooks::*;
pub use guard::GuardVerdict;
pub use loader::*;
pub use manifest::*;
pub use schema::validate_tool_args;
pub use state::*;
