//! Opt-in, stateless OpenAI Responses protocol helpers. No transport or credentials.
#[cfg(feature = "secrets")]
pub mod chatgpt_tokens;
pub mod request;
