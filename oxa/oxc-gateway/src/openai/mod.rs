//! Opt-in, stateless OpenAI Responses protocol helpers. No transport or credentials.
pub mod chatgpt_request;
#[cfg(feature = "secrets")]
pub mod chatgpt_tokens;
pub mod request;
