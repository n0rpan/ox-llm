//! Opt-in, engine-independent cloud gateway building blocks.
#![forbid(unsafe_code)]
#[cfg(feature = "upstream-anthropic-request")]
pub mod anthropic_request;
#[cfg(feature = "policy")]
pub mod policy;
#[cfg(feature = "secrets")]
pub mod redaction;
#[cfg(feature = "secrets")]
pub mod secrets;
