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
