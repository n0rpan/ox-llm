//! Architect-owned: the selector module list and the shared outcome type (complete). The
//! selectors are tasks D7 (model, thinking, theme, settings), D8 (sessions), D9 (tree) and
//! F7 (mcp).
//!
//! Every selector is a plain struct built from data, with three methods of the same shape:
//! `key(&mut self, key, keymap) -> Outcome<T>`, `render(&mut self, width, theme) -> Vec<String>`
//! and, where text is typed, `paste`. None of them touches the session, the store or the
//! settings files: the result is data, and the app (D1) turns it into `AppAction`s.
mod mcp;
mod model;
mod sessions;
mod settings;
mod theme;
mod thinking;
mod tree;

pub use mcp::*;
pub use model::*;
pub use sessions::*;
pub use settings::*;
pub use theme::*;
pub use thinking::*;
pub use tree::*;

/// The state of a selector after a key.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome<T> {
    /// Still open; the key was not used.
    Ignored,
    /// Still open; redraw.
    Pending,
    /// Closed with a result.
    Done(T),
    /// Closed without a result (Escape).
    Cancelled,
}

impl<T> Outcome<T> {
    /// `true` when the selector closed.
    pub fn closed(&self) -> bool {
        matches!(self, Outcome::Done(_) | Outcome::Cancelled)
    }
}
