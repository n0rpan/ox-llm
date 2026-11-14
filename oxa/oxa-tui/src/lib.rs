//! W0-FROZEN (architect-owned).
//!
//! # oxa-tui: the terminal UI library (a port of pi-tui)
//!
//! pi-tui's model, kept as is: **a component renders lines for a width; the renderer diffs
//! lines** and writes the minimal byte stream. Nothing in this crate knows about agents,
//! sessions or the engine; it depends on `oxa-proto` (for [`oxa_proto::KeyId`]) and
//! `oxa-config` (for `Keymap`) only.
//!
//! ## Contracts shared by every module
//! * **A rendered line** is a `String` of printable text plus SGR (`ESC [ .. m`) and OSC 8
//!   sequences, with no `\n`, `\r` or cursor movement, and `visible_width(line) <= width`.
//!   A component that returns a wider line has a bug; the renderer truncates defensively.
//! * **Width is measured in terminal columns** by [`visible_width`] (wide = 2, combining = 0,
//!   escape sequences = 0). No other width function may be used anywhere.
//! * **Every line is self-contained**: styles opened on a line are closed on it (`ESC[0m`),
//!   so lines can be diffed, sliced and scrolled independently.
//! * **No hidden state in rendering**: `render(width, theme)` twice with no input in between
//!   returns equal lines. Time (spinners) enters only through explicit `tick` methods.
//! * **No panics on input**: any byte sequence from the terminal and any (partial, streaming)
//!   Markdown must be handled without panicking.
//! * **Terminal restore**: raw mode, alt screen, mouse and keyboard-protocol flags are undone on
//!   drop and by the panic hook ([`install_panic_hook`]).
//! * Tests never need a real terminal: [`TestTerminal`] records output bytes and feeds input.
//!
//! Threads: one UI thread owns the `Terminal` and all components. Nothing here is `Sync`.

mod autocomplete;
mod builtin_themes;
mod caps;
pub mod components;
mod editor;
mod fuzzy;
mod gfx;
mod highlight;
mod image;
mod keys;
mod markdown;
mod omarchy;
mod osc;
mod render;
mod term;
mod testterm;
mod theme;
mod theme_core;
mod width;

pub use autocomplete::*;
pub use builtin_themes::*;
pub use caps::*;
pub use components::*;
pub use editor::*;
pub use fuzzy::*;
pub use gfx::*;
pub use highlight::*;
pub use image::*;
pub use keys::*;
pub use markdown::*;
pub use omarchy::*;
pub use osc::*;
pub use render::*;
pub use term::*;
pub use testterm::*;
pub use theme::*;
pub use theme_core::*;
pub use width::*;

pub use oxa_proto::KeyId;
