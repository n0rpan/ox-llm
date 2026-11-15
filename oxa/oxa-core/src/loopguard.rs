//! Degenerate-repetition detector for streamed assistant text (issue #141).
//!
//! A model can fall into a loop where its answer is one paragraph repeated hundreds of times
//! (DeepSeek V4 Pro: 192 copies, 148 KB, nothing noticed). [`LoopDetector`] watches the streamed
//! text and reports when the same block of at least [`MIN_BLOCK`] bytes has been written
//! [`MIN_REPEATS`] times in a row, directly one after the other.
//!
//! ## Cost
//! Only a bounded tail ([`WINDOW`] bytes) is kept, and the check runs at most once per
//! [`CHECK_STEP`] new bytes, so the work per delta is constant, never a rescan of the whole text.
//!
//! ## Rule
//! The text ends with `B B B` where `B` is any block of `MIN_BLOCK..=MAX_BLOCK` bytes, and `B` is
//! not itself made of a short period (a rule of dashes, a run of identical short lines, padding):
//! those are ordinary text and code, never reported.

/// Smallest repeated block, in bytes.
pub const MIN_BLOCK: usize = 200;
/// Largest repeated block that is searched for, in bytes.
pub const MAX_BLOCK: usize = 8 * 1024;
/// Consecutive copies of the block that make a loop.
pub const MIN_REPEATS: usize = 3;
/// New bytes between two checks.
pub const CHECK_STEP: usize = 128;
/// A block with a period up to this many bytes is a short repetition (separator, repeated line).
pub const SHORT_PERIOD: usize = 120;
/// Bytes of tail kept.
const WINDOW: usize = MAX_BLOCK * MIN_REPEATS;

/// Incremental detector. Feed every text delta to [`LoopDetector::push`].
#[derive(Debug, Default, Clone)]
pub struct LoopDetector {
    tail: Vec<u8>,
    since_check: usize,
    found: bool,
}

impl LoopDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything (a new reply starts).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Add a streamed delta. Returns true once a loop is seen (and stays true).
    pub fn push(&mut self, delta: &str) -> bool {
        if self.found {
            return true;
        }
        self.tail.extend_from_slice(delta.as_bytes());
        self.since_check += delta.len();
        if self.tail.len() > 2 * WINDOW {
            let cut = self.tail.len() - WINDOW;
            self.tail.drain(..cut);
        }
        if self.since_check >= CHECK_STEP {
            self.since_check = 0;
            self.found = repeats_at_end(&self.tail);
        }
        self.found
    }
}

/// Whether `text` ends with a block of `MIN_BLOCK..=MAX_BLOCK` bytes repeated `MIN_REPEATS`
/// times in a row. Pure; bounded by the last `WINDOW` bytes.
pub fn repeats_at_end(text: &[u8]) -> bool {
    let text = &text[text.len().saturating_sub(WINDOW)..];
    let n = text.len();
    if n < MIN_BLOCK * MIN_REPEATS {
        return false;
    }
    let needle = &text[n - MIN_BLOCK..];
    let max_p = (n / MIN_REPEATS).min(MAX_BLOCK);
    // Earlier occurrences of the last MIN_BLOCK bytes give the candidate periods.
    let mut p = MIN_BLOCK;
    while p <= max_p {
        if text[n - p - MIN_BLOCK..n - p] == *needle
            && text[n - 2 * p..n - p] == text[n - p..]
            && text[n - 3 * p..n - 2 * p] == text[n - p..]
            && !short_periodic(&text[n - p..])
        {
            return true;
        }
        p += 1;
    }
    false
}

/// Whether `block` repeats with a period of at most `SHORT_PERIOD` bytes.
fn short_periodic(block: &[u8]) -> bool {
    (1..=SHORT_PERIOD.min(block.len() - 1)).any(|q| block[q..] == block[..block.len() - q])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(seed: usize) -> String {
        (0..40).map(|i| format!("word{} ", (i * 7 + seed) % 97)).collect()
    }

    fn feed(d: &mut LoopDetector, s: &str, chunk: usize) -> Option<usize> {
        let cs: Vec<char> = s.chars().collect();
        let mut at = 0;
        for c in cs.chunks(chunk) {
            at += c.len();
            if d.push(&c.iter().collect::<String>()) {
                return Some(at);
            }
        }
        None
    }

    #[test]
    fn detects_repeated_paragraph() {
        let p = para(1);
        assert!(p.len() >= MIN_BLOCK);
        let text = format!("Intro text.\n{}", p.repeat(5));
        let mut d = LoopDetector::new();
        let at = feed(&mut d, &text, 1000).expect("loop");
        assert!(at < text.len());
    }

    #[test]
    fn detects_in_tiny_chunks_and_stops_early() {
        let p = para(2);
        let text = p.repeat(50);
        let mut d = LoopDetector::new();
        let at = feed(&mut d, &text, 3).expect("loop");
        assert!(at <= 3 * p.len() + CHECK_STEP + 3, "detected at {at}");
        assert!(d.push("more"), "stays detected");
    }

    #[test]
    fn two_copies_are_not_a_loop() {
        let text = format!("{}{}{}", para(1), para(1), para(3));
        let mut d = LoopDetector::new();
        assert!(feed(&mut d, &text, 50).is_none());
    }

    #[test]
    fn near_miss_is_not_a_loop() {
        let mut text = String::new();
        for i in 0..6 {
            // Same paragraph but one word differs each time.
            text.push_str(&para(1));
            text.push_str(&format!("variant{i} "));
        }
        let mut d = LoopDetector::new();
        assert!(feed(&mut d, &text, 40).is_none());
    }

    #[test]
    fn normal_long_text_is_not_a_loop() {
        let text: String = (0..400).map(para).collect::<Vec<_>>().join("\n\n");
        assert!(text.len() > 50_000);
        let mut d = LoopDetector::new();
        assert!(feed(&mut d, &text, 17).is_none());
    }

    #[test]
    fn repeated_short_lines_and_separators_are_not_a_loop() {
        let lines = "    println!(\"x\");\n".repeat(100);
        let rule = "-".repeat(3000);
        let blanks = "\n".repeat(2000);
        for t in [lines, rule, blanks] {
            let mut d = LoopDetector::new();
            assert!(feed(&mut d, &t, 25).is_none());
        }
    }

    #[test]
    fn multibyte_text_works() {
        let p: String = (0..30).map(|i| format!("æøå{i} ünï — 漢字 ")).collect();
        let mut d = LoopDetector::new();
        assert!(feed(&mut d, &p.repeat(4), 7).is_some());
    }

    #[test]
    fn reset_forgets() {
        let mut d = LoopDetector::new();
        assert!(feed(&mut d, &para(1).repeat(4), 100).is_some());
        d.reset();
        assert!(!d.push("hello"));
    }
}
