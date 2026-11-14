//! Architect-owned: the in-memory terminal for golden byte-stream tests (complete). Not a task file.
use crate::{InputEvent, TermCaps, TermMode, Terminal};
use std::collections::VecDeque;

/// An in-memory [`Terminal`]: records every output byte, replays scripted input.
///
/// Golden renderer tests compare [`TestTerminal::take_output`] between frames. Mode switches
/// are recorded in [`TestTerminal::modes`] and are **not** written to the output bytes, so
/// renderer goldens do not depend on mode sequences.
#[derive(Debug, Clone)]
pub struct TestTerminal {
    /// All bytes written since the last [`TestTerminal::take_output`].
    pub out: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
    pub caps: TermCaps,
    /// Batches returned by successive `poll` calls.
    pub input: VecDeque<Vec<InputEvent>>,
    /// Every `set_mode` call, in order.
    pub modes: Vec<(TermMode, bool)>,
    pub flushes: usize,
    pub suspended: bool,
    /// Number of suspend calls.
    pub suspends: usize,
}

impl TestTerminal {
    pub fn new(cols: u16, rows: u16) -> TestTerminal {
        TestTerminal {
            out: Vec::new(),
            cols: cols.max(1),
            rows: rows.max(1),
            caps: TermCaps::BASIC,
            input: VecDeque::new(),
            modes: Vec::new(),
            flushes: 0,
            suspended: false,
            suspends: 0,
        }
    }
    /// The bytes written since the last call, as a lossy string, and clear them.
    pub fn take_output(&mut self) -> String {
        let s = String::from_utf8_lossy(&self.out).into_owned();
        self.out.clear();
        s
    }
    /// Queue one batch of events for the next `poll`.
    pub fn push_input(&mut self, events: Vec<InputEvent>) {
        self.input.push_back(events);
    }
    /// Change the size and queue the matching `Resize` event.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self.input.push_back(vec![InputEvent::Resize {
            cols: self.cols,
            rows: self.rows,
        }]);
    }
    /// `true` when the last `set_mode(mode, ..)` call switched it on.
    pub fn mode_on(&self, mode: TermMode) -> bool {
        self.modes
            .iter()
            .rev()
            .find(|(m, _)| *m == mode)
            .map(|(_, on)| *on)
            .unwrap_or(false)
    }
}

impl Terminal for TestTerminal {
    fn write(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }
    fn flush(&mut self) {
        self.flushes += 1;
    }
    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
    fn poll(&mut self, _timeout_ms: u64) -> Vec<InputEvent> {
        self.input.pop_front().unwrap_or_default()
    }
    fn caps(&self) -> TermCaps {
        self.caps
    }
    fn set_mode(&mut self, mode: TermMode, on: bool) {
        self.modes.push((mode, on));
    }
    fn suspend(&mut self) {
        self.suspended = true;
        self.suspends += 1;
    }
    fn resume(&mut self) {
        self.suspended = false;
    }
}
