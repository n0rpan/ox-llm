#![forbid(unsafe_code)]

#[cfg(feature = "upstream-openai-chat")]
pub mod openai_chat;
#[cfg(feature = "upstream-openai-chat")]
pub mod openai_contract;
