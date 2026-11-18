//! Owner standing data and action guards. Never reads or writes runner task state.
// Public Debug remains available without recursively exposing protected payloads.
macro_rules! private_debug {
    ($($value:ty),+ $(,)?) => {$(
        impl std::fmt::Debug for $value {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!(stringify!($value), "(<private>)"))
            }
        }
    )+};
}
mod guard;
pub use guard::{Action, Refusal, RefusalReason, Scope, Subject};
mod types;
pub use types::{
    Authority, Condition, Entry, EntryState, GateEvidence, Kind, Reminder, ReservedClass, ReviewTier, Rule,
    Source, Value,
};
mod store;
pub use store::{GuardError, Guarded, Read, Standing, Verification};
mod asks;
pub use asks::{Answer, AnswerRequest, Ask, Choice, Proposal, Question, QuestionOption};
