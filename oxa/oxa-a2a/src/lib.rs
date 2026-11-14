//! Independently implemented A2A 0.3.0 JSON-RPC wire profile.
//!
//! This crate is inert unless explicitly used. It grants no permissions and
//! accepts no executable/file parts. See README.md for scope and references.
pub mod loopback;
pub mod mock;
pub mod server;
pub mod sse;
pub mod wire;
