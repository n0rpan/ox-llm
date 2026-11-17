//! Opt-in, engine-independent cloud gateway building blocks.
#![forbid(unsafe_code)]
#[cfg(feature = "anthropic-gateway")]
mod anthropic_replay;
#[cfg(feature = "anthropic-gateway")]
pub mod gateway;
#[cfg(feature = "anthropic-gateway")]
mod history;
#[cfg(feature = "anthropic-gateway")]
pub mod runtime;
#[cfg(feature = "share")]
pub mod share;

#[cfg(feature = "cloud-only")]
pub mod catalog;
#[cfg(feature = "ledger")]
pub mod ledger;
#[cfg(feature = "upstream-openai")]
pub mod openai;
#[cfg(all(feature = "anthropic-client", feature = "ledger"))]
pub mod opencode;
#[cfg(feature = "policy")]
pub mod policy;
#[cfg(feature = "pricing")]
pub mod pricing;
#[cfg(all(feature = "anthropic-client", feature = "ledger"))]
pub mod rate_limits;

#[cfg(feature = "accounting")]
pub mod accounting;
#[cfg(feature = "accounting")]
pub mod billing;
#[cfg(feature = "accounting")]
mod reservations;
#[cfg(feature = "accounting")]
pub mod spend;

#[cfg(feature = "secrets")]
pub mod redaction;
#[cfg(feature = "secrets")]
pub mod secrets;

#[cfg(feature = "upstream-anthropic-request")]
pub mod anthropic_request;
#[cfg(feature = "upstream-anthropic-stream")]
pub mod anthropic_stream;

#[cfg(feature = "anthropic-client")]
pub mod anthropic_client;
#[cfg(feature = "anthropic-client")]
mod sse;

#[cfg(all(feature = "anthropic-client", feature = "pricing"))]
pub mod openrouter;
#[cfg(all(feature = "anthropic-client", feature = "pricing"))]
pub mod status;

/// Private, opt-in trusted-terminal diagnostic facts.
pub mod diagnostics;
