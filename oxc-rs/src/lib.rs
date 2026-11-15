#![forbid(unsafe_code)]

#[cfg(feature = "upstream-anthropic-request")]
pub use oxc_gateway::anthropic_request;
