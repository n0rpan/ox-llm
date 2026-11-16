//! Opt-in, stateless OpenAI Responses protocol helpers. No transport or credentials.
pub mod chatgpt_request;
#[cfg(feature = "secrets")]
pub mod chatgpt_tokens;
#[cfg(feature = "anthropic-client")]
pub mod models;
pub mod request;
