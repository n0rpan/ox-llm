//! Opt-in, stateless OpenAI Responses helpers and explicitly configured transport.
#[cfg(all(feature = "anthropic-client", feature = "ledger"))]
pub mod client;

pub mod chatgpt_request;
#[cfg(feature = "secrets")]
pub mod chatgpt_tokens;
pub mod request;
pub mod stream;
