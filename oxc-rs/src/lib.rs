#![forbid(unsafe_code)]
#![cfg(feature = "native-controller")]

//! Opt-in controller foundations. No daemon or model engine is exposed here.
pub mod http_gate;
pub mod paths;
pub mod transport;
