#![forbid(unsafe_code)]

#[cfg(feature = "upstream-openai-responses")]
pub mod openai_contract;
#[cfg(feature = "upstream-openai-responses")]
pub mod openai_responses;
