//! Inert worker supervisor; no listener or subprocess is started on import.
//! The initial executable profile is local, tool-free oxa over bounded RPC.
pub mod a2a_run;
pub mod cli;
mod journal;
mod process;
pub mod recovery;
mod recovery_cli;
pub mod review;
mod review_cli;
pub mod rpc_agent;
mod run;
pub mod workspace;

pub use run::*;
