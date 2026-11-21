//! The lead kanban: one owner-private board of planned work per repository, with typed
//! operations, an undo log and per-actor authority. It is the pull-based work list the lead
//! keeps full and workers take from; it is not the GitHub PR policy board (`crate::board`).
//!
//! This slice (K1) is the store, the operations and undo. The dispatcher, reconciler, web
//! page and CLI land in later slices and call the same typed operations. Nothing here starts
//! a worker, makes a GitHub call or reads a setting; dispatch defaults to `Paused`.
// Public Debug stays available without recursively exposing owner work-item text.
macro_rules! private_debug {
    ($($value:ty),+ $(,)?) => {$(
        impl std::fmt::Debug for $value {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!(stringify!($value), "(<private>)"))
            }
        }
    )+};
}

mod model;
mod ops;
mod store;

pub use crate::standing::Authority;
pub use model::{
    Actor, Applied, Capacity, Cause, Column, Dispatch, EditFields, Error, Goal, HostId, HostPref, Item,
    Kanban, Measure, MeasureKind, MeasureResult, Need, NewChild, Op, Pr, Run, Stuck,
};
pub use store::Store;
