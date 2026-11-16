//! Default-off foundations for the project orchestrator (#282).
//!
//! Enabling `core` provides explicit private audit storage and approved text snapshots. Importing this crate
//! never opens files, starts threads/processes or creates a listener. No CLI
//! dependency or runtime hook is installed by this foundational slice.
#[cfg(all(feature = "core", target_os = "linux"))]
mod journal;
#[cfg(all(feature = "core", target_os = "linux"))]
mod namespace;
#[cfg(all(feature = "core", target_os = "linux"))]
mod ownership;

#[cfg(all(feature = "core", target_os = "linux"))]
pub use journal::Journal as AuditJournal;

#[cfg(all(feature = "core", target_os = "linux"))]
pub mod workspace;
