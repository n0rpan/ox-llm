//! Opt-in, stateless OpenAI Responses helpers and explicitly configured transport.
#[cfg(all(feature = "anthropic-client", feature = "ledger"))]
pub mod client;

pub mod chatgpt_request;
#[cfg(feature = "secrets")]
pub mod chatgpt_tokens;
#[cfg(feature = "anthropic-client")]
pub mod models;
pub mod request;
pub mod stream;
