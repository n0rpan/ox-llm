//! Shared HTTP pieces of oxa. See the README.
#![forbid(unsafe_code)]

#[cfg(feature = "client")]
pub mod client;
pub mod server;
pub mod sse;
