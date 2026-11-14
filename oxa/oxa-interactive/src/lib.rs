//! W0-FROZEN (architect-owned).
//!
//! # oxa-interactive: the interactive mode
//!
//! The terminal front end over `oxa_core::Session`: pi's interactive mode in its two layouts
//! (regular = main screen with scrollback, fullscreen = alternate screen with a viewport).
//!
//! ## Architecture (frozen)
//! ```text
//!  UI thread (owns Terminal, Renderer, ViewModel, all components)
//!    loop: Terminal::poll -> keys -> Editor / DialogHost / selector / slash -> AppAction
//!          worker events  -> apply_event(ViewModel)         (pure reducer, app.rs)
//!          UiRequest      -> DialogHost                      (dialogs.rs)
//!          ViewModel -> blocks -> lines -> Renderer::draw    (views/*, chrome.rs, regular/fullscreen)
//!  Worker thread (owns the Session)
//!    loop: SessionCmd from the UI thread -> Session::{prompt, compact, navigate, ..}
//!          events out through an mpsc EventSink; dialogs through UiProxy (blocks the worker)
//!  Any thread: oxa_core::Handle::{steer, follow_up, abort} (Send + Clone)
//! ```
//! Data flows one way: `AgentEvent` -> [`ViewModel`] -> lines. Views never call the session.
//!
//! ## Contracts
//! * **Everything the tests need is pure**: [`apply_event`], [`frames`], the view functions,
//!   [`Viewport`], the selectors and the dialogs render from plain data with a `Theme` and a
//!   width; no test needs a terminal or an engine.
//! * **The wire is not ours.** This crate only observes events and sends commands; it never
//!   builds or edits messages (W1-W3). Mode, status, footer and notices are display only and
//!   never reach the wire (W3: no mode text).
//! * **`Ui` from the worker thread** goes through [`UiProxy`] -> [`UiRequest`] -> [`DialogHost`];
//!   a dismissed or timed-out dialog answers `None` / `Answer::Deny`, never consent.
//! * Keys only through `oxa_config::Keymap` (`app.*` and `tui.*` ids).
//! * Session swaps (`/new`, `/resume`, fork) go through [`InteractiveOpts::reopen`]; the app
//!   never builds `Deps` itself.

mod app;
mod chrome;
mod clipboard;
mod dialogs;
mod draft_trigger;
mod driver;
mod external;
mod fullscreen;
mod model;
mod regular;
mod search;
mod select;
pub mod selectors;
pub mod skin;
mod slash;
pub mod views;

pub use app::*;
pub use chrome::*;
pub use clipboard::*;
pub use dialogs::*;
pub use driver::{ask_trust_interactive, ask_trust_on};
pub use external::*;
pub use fullscreen::*;
pub use model::*;
pub use regular::*;
pub use search::*;
pub use select::*;
pub use selectors::*;
pub use skin::*;
pub use slash::*;
pub use views::*;
