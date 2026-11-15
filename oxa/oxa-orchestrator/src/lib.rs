//! Inert worker supervisor; no listener or subprocess is started on import.
//! The initial executable profile is local, tool-free oxa over bounded RPC.
pub mod a2a_run;
mod agent_cli;
#[cfg(feature = "interactive-control")]
pub mod background;
#[cfg(feature = "interactive-control")]
mod background_process;
mod checkpoint;
pub mod cli;
pub mod control;
mod git_worktrees;
#[cfg(feature = "interactive-control")]
mod goal_agent;
pub mod goal_session;
pub mod goals;
mod journal;
pub mod lineage;
mod namespace;
mod ownership;
mod process;
pub mod recovery;
mod recovery_cli;
pub mod registration;
pub mod resume;
pub mod review;
mod review_cli;
pub mod rpc_agent;
mod run;
#[cfg(feature = "interactive-control")]
pub mod tui;
mod usage;
#[cfg(feature = "interactive-control")]
pub mod worker_lanes;
#[cfg(feature = "interactive-control")]
mod worker_profiles;
pub mod workspace;

pub use run::*;
