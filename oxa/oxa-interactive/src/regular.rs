//! D1: the regular (main screen) layout. Owner: task D1. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/tui-main-screen.ts and coding-agent interactive-mode.ts (regular mode)
use crate::app::{transcript_lines_in, ViewEnv};
use crate::{ViewModel, ViewSettings};
use oxa_tui::{CursorPos, Theme};

/// What to do with the terminal for one step of the regular layout.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegularStep {
    /// Lines to commit to scrollback (`Renderer::commit`): blocks that just finished.
    pub commit: Vec<String>,
    /// The live region (`Frame::lines`): unfinished blocks, loader, widgets, editor, footer.
    pub live: Vec<String>,
    pub cursor: Option<CursorPos>,
}

/// Tracks which blocks were already printed.
///
/// Contract (acceptance, plan D1): **finished blocks are printed exactly once**, in order. A
/// block is committed when it and every block before it are finished (`Block::finished`).
/// After a commit a block is never printed again, also across resizes. A model whose block
/// list shrank (session swap, `/tree`) resets the printer: the caller clears the screen and
/// all blocks are committed again.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegularPrinter {
    /// Number of blocks already committed.
    pub committed: usize,
    /// A committed block rendered at least one line (the next block gets a separator).
    printed_any: bool,
}

impl RegularPrinter {
    pub fn new() -> RegularPrinter {
        RegularPrinter::default()
    }
    /// Lines to commit now and the transcript part of the live region (unfinished blocks).
    /// The caller appends loader, widgets, editor and footer lines to `live`.
    pub fn step(
        &mut self,
        vm: &ViewModel,
        width: u16,
        theme: &Theme,
        settings: &ViewSettings,
    ) -> RegularStep {
        self.step_in(vm, width, theme, settings, &ViewEnv::default())
    }
    /// [`RegularPrinter::step`] with the app's view environment (capabilities, hooks, keymap).
    pub(crate) fn step_in(
        &mut self,
        vm: &ViewModel,
        width: u16,
        theme: &Theme,
        settings: &ViewSettings,
        env: &ViewEnv,
    ) -> RegularStep {
        if self.needs_reset(vm) {
            self.reset();
        }
        let n = vm.blocks.len();
        let first = self.committed.min(n);
        // Only the blocks not yet printed are rendered: this runs every frame, and the printed
        // history must cost nothing (a long session would otherwise be re-rendered at 60 Hz).
        let (mut lines, starts) =
            transcript_lines_in(vm, first, self.printed_any, width, theme, settings, env);
        // The leading run of finished blocks; a block never un-finishes, so this only grows.
        let k = first + vm.blocks[first..].iter().take_while(|b| b.finished()).count();
        let split = if k < n { starts[k - first] } else { lines.len() };
        let mut step = RegularStep {
            live: lines.split_off(split),
            ..RegularStep::default()
        };
        if k > self.committed {
            self.printed_any |= !lines.is_empty();
            step.commit = lines;
            self.committed = k;
        }
        step
    }
    /// `true` when the model's blocks no longer extend what was committed (swap or rewind).
    pub fn needs_reset(&self, vm: &ViewModel) -> bool {
        self.committed > vm.blocks.len()
    }
    pub fn reset(&mut self) {
        self.committed = 0;
        self.printed_any = false;
    }
}
