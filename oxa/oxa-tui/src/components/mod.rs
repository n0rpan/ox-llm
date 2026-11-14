//! Architect-owned: the component module list plus the two trivial components (complete).
//! The widgets themselves are task B12 (one file each).
// ported from pi packages/tui/src/components/{spacer,stack}.ts
use crate::{Component, Key, Theme};

mod border;
mod boxed;
mod input;
mod loader;
mod scrollbar;
mod select_list;
mod text;

pub use border::*;
pub use boxed::*;
pub use input::*;
pub use loader::*;
pub use scrollbar::*;
pub use select_list::*;
pub use text::*;

/// `lines` empty lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spacer {
    pub lines: usize,
}

impl Component for Spacer {
    fn render(&mut self, _width: u16, _theme: &Theme) -> Vec<String> {
        vec![String::new(); self.lines]
    }
}

/// Children rendered top to bottom. Keys go to the child at `focus`, if any.
#[derive(Default)]
pub struct Container {
    pub children: Vec<Box<dyn Component>>,
    pub focus: Option<usize>,
}

impl Container {
    pub fn new() -> Container {
        Container::default()
    }
    pub fn push(&mut self, c: Box<dyn Component>) {
        self.children.push(c);
    }
    pub fn clear(&mut self) {
        self.children.clear();
        self.focus = None;
    }
}

impl Component for Container {
    fn render(&mut self, width: u16, theme: &Theme) -> Vec<String> {
        let mut out = Vec::new();
        for c in &mut self.children {
            out.extend(c.render(width, theme));
        }
        out
    }
    fn input(&mut self, key: &Key) -> bool {
        match self.focus.and_then(|i| self.children.get_mut(i)) {
            Some(c) => c.input(key),
            None => false,
        }
    }
    fn invalidate(&mut self) {
        for c in &mut self.children {
            c.invalidate();
        }
    }
}
