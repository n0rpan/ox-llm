//! Default-off foundations for the project orchestrator (#282).
//!
//! Enabling `core` provides explicit private audit storage and bounded child-pipe transport. Importing this crate
//! never opens files, starts threads/processes or creates a listener. No CLI
//! dependency or runtime hook is installed by this foundational slice.
#[cfg(feature = "core")]
mod journal;
#[cfg(feature = "core")]
mod namespace;
#[cfg(feature = "core")]
mod ownership;

#[cfg(feature = "core")]
pub use journal::Journal as AuditJournal;

#[cfg(feature = "core")]
pub mod pipes;

#[cfg(feature = "core")]
pub mod native;

#[cfg(feature = "core")]
pub mod rpc;

#[cfg(feature = "core")]
pub mod supervisor;

#[cfg(feature = "core")]
pub mod recovery;

#[cfg(feature = "core")]
pub mod rpc_agent;

#[cfg(feature = "core")]
pub mod workspace;

#[cfg(feature = "core")]
pub mod workspace_jobs;

#[cfg(feature = "core")]
pub mod a2a_run;

#[cfg(feature = "core")]
mod checkpoint;

#[cfg(feature = "core")]
pub mod a2a_recovery;

#[cfg(feature = "core")]
mod a2a_reconcile;

#[cfg(feature = "core")]
pub mod resume;
