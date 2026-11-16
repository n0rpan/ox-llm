//! Opt-in, stateless OpenAI Responses protocol helpers. No transport or credentials.
#[cfg(feature = "anthropic-client")]
pub mod models;
pub mod request;
