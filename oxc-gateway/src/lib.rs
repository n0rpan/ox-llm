//! Opt-in, engine-independent cloud gateway building blocks.
#![forbid(unsafe_code)]
#[cfg(feature = "cloud-only")]
pub mod catalog;
#[cfg(feature = "policy")]
pub mod policy;
#[cfg(feature = "secrets")]
pub mod redaction;
#[cfg(feature = "secrets")]
pub mod secrets;
