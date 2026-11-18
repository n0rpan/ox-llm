//! Default-off foundations for the project orchestrator (#282).
//!
//! Enabling `core` provides explicit private audit storage and approved text snapshots. Importing this crate
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
pub mod workspace;

#[cfg(feature = "a2a")]
pub mod follow;

#[cfg(feature = "a2a")]
pub mod control;
