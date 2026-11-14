//! Inert worker supervisor; no listener or subprocess is started on import.
//! The initial executable profile is local, tool-free oxa over bounded RPC.
mod journal;
mod process;
mod run;

pub use run::*;
