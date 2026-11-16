//! Explicitly opt-in guest access. Importing this module starts no listener or worker.
pub mod admission;
pub mod keys;
pub mod listener;
mod protocol;
pub mod tailnet;
