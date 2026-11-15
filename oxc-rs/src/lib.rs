#![forbid(unsafe_code)]

#[cfg(feature = "upstream-anthropic-request")]
pub mod anthropic_request;

#[cfg(feature = "upstream-anthropic-stream")]
pub mod anthropic_stream;
