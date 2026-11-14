//! Inert worker supervisor; no listener or subprocess is started on import.
//! The initial executable profile is local, tool-free oxa over bounded RPC.
pub mod a2a_run;
mod journal;
mod process;
pub mod rpc_agent;
mod run;

pub use run::*;
