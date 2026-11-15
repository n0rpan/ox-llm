//! Opt-in, engine-independent cloud gateway building blocks.
#![forbid(unsafe_code)]
#[cfg(feature = "cloud-only")]
pub mod catalog;
#[cfg(feature = "ledger")]
pub mod ledger;
#[cfg(feature = "policy")]
pub mod policy;
#[cfg(feature = "pricing")]
pub mod pricing;
#[cfg(feature = "runtime")]
pub mod runtime;

#[cfg(feature = "accounting")]
pub mod accounting;
#[cfg(feature = "accounting")]
pub mod spend;

#[cfg(feature = "secrets")]
pub mod redaction;
#[cfg(feature = "secrets")]
pub mod secrets;
