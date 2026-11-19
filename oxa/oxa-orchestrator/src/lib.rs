//! Foundations for the project orchestrator (#282).
//!
//! Provides explicit private audit storage, approved text snapshots, the bounded control request
//! decoder and the follow history. Importing this crate never opens files, starts threads/processes
//! or creates a listener; callers decide when to use it. Run-time use is gated by the user-only
//! setting `oxa.a2a.enabled`.
mod journal;
mod namespace;
mod ownership;

pub use journal::Journal as AuditJournal;

pub mod workspace;

pub mod follow;

pub mod control;
