//! Opt-in, stateless OpenAI Responses protocol helpers. No transport or credentials.
#[cfg(feature = "secrets")]
pub mod chatgpt_tokens;
#[cfg(feature = "anthropic-client")]
pub mod models;
pub mod request;
