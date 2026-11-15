#![forbid(unsafe_code)]

#[cfg(feature = "upstream-anthropic-request")]
pub use oxc_gateway::anthropic_request;

#[cfg(feature = "upstream-anthropic-stream")]
pub mod anthropic_stream;
