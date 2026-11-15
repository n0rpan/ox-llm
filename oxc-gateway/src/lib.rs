//! Opt-in, engine-independent cloud gateway building blocks.
#![forbid(unsafe_code)]
#[cfg(feature = "openrouter")]
pub mod openrouter;
#[cfg(feature = "policy")]
pub mod policy;
#[cfg(feature = "secrets")]
pub mod redaction;
#[cfg(feature = "secrets")]
pub mod secrets;
