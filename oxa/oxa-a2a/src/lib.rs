//! Independently implemented A2A 0.3.0 JSON-RPC wire profile.
//!
//! This crate is inert unless explicitly used. It grants no permissions and
//! accepts no executable/file parts. See README.md for scope and references.
#[cfg(feature = "agent-host")]
pub mod collect;
pub mod loopback;
pub mod mock;
#[cfg(feature = "agent-host")]
pub mod server;
pub mod sse;
pub mod wire;
