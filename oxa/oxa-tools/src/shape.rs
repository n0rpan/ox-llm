//! A5: `shape`, the deterministic output cap (W4).
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

/// Smallest cap the shaper honours; smaller values are raised to this.
const MIN_CAP: usize = 256;
/// The two newlines that surround the mark line.
const NEWLINES: usize = 2;

/// Shape raw tool output for the wire.
///
/// 1. Invalid UTF-8 is replaced lossily; ANSI escape sequences are removed ([`strip_ansi`]);
///    `\r\n` becomes `\n`.
/// 2. If the result is at most `cap` bytes it is returned unchanged.
/// 3. Otherwise the full, unshaped `raw` bytes are written to `<spill_dir>/<sha256 hex of
///    raw>.txt` (0600; an existing file is left alone) and the result is
///    `head + "\n" + MARK + "\n" + tail` where `MARK` is the one fixed line
///    `[output truncated: <total> bytes, full output in <spill path>]`, and head/tail are cut
///    at line boundaries where possible and never inside a UTF-8 sequence or a grapheme
///    joined by ZWJ. `keep_tail = true` gives the tail three quarters of the budget (bash),
///    otherwise the head gets three quarters.
///
/// Guarantees: output length <= `cap`; valid UTF-8; deterministic (same `raw`, `cap`,
/// `keep_tail`, `spill_dir` -> same bytes); the spill file holds every byte of `raw`. If the
/// spill file cannot be written the mark says `full output not saved` and the function still
/// returns. `cap` below 256 is treated as 256.
pub fn shape(raw: &[u8], cap: usize, keep_tail: bool, spill_dir: &Path) -> String {
    let cap = cap.max(MIN_CAP);
    let text = strip_ansi(&String::from_utf8_lossy(raw)).replace("\r\n", "\n");
    if text.len() <= cap {
        return text;
    }

    let spill_path = spill_dir.join(spill_file_name(raw));
    let saved = spill(raw, spill_dir, &spill_path);
    let mut mark = if saved {
        format!(
            "[output truncated: {} bytes, full output in {}]",
            raw.len(),
            spill_path.display()
        )
    } else {
        format!("[output truncated: {} bytes, full output not saved]", raw.len())
    };
    if mark.len() + NEWLINES > cap {
        // A very long spill path must not push the output past the cap: fall back to the short
        // mark, which always fits (cap >= 256).
        mark = format!("[output truncated: {} bytes]", raw.len());
    }

    let budget = cap - mark.len() - NEWLINES;
    let (head_max, tail_max) = if keep_tail {
        let tail = budget * 3 / 4;
        (budget - tail, tail)
    } else {
        let head = budget * 3 / 4;
        (head, budget - head)
    };
    let head = head_part(&text, head_max);
    let tail = tail_part(&text, tail_max);

    let mut out = String::with_capacity(head.len() + mark.len() + tail.len() + NEWLINES);
    out.push_str(head);
    out.push('\n');
    out.push_str(&mark);
    out.push('\n');
    out.push_str(tail);
    out
}

/// Remove CSI, OSC and other escape sequences and lone control characters except `\n` and `\t`.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => skip_escape(&mut it),
            '\u{9b}' => skip_csi_body(&mut it),
            '\u{9d}' => skip_string_body(&mut it),
            '\n' | '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

/// Called after ESC has been consumed.
fn skip_escape(it: &mut Chars<'_>) {
    match it.peek().copied() {
        Some('[') => {
            it.next();
            skip_csi_body(it);
        }
        Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
            it.next();
            skip_string_body(it);
        }
        Some(c) if ('\u{20}'..='\u{2f}').contains(&c) => {
            // nF sequence: intermediates, then one final byte.
            while matches!(it.peek(), Some(c) if ('\u{20}'..='\u{2f}').contains(c)) {
                it.next();
            }
            if matches!(it.peek(), Some(c) if ('\u{30}'..='\u{7e}').contains(c)) {
                it.next();
            }
        }
        Some(c) if ('\u{30}'..='\u{7e}').contains(&c) => {
            // Fe / Fs / Fp single-character escape.
            it.next();
        }
        _ => {}
    }
}

/// CSI parameters and intermediates, then the final byte. Stops before a newline.
fn skip_csi_body(it: &mut Chars<'_>) {
    while matches!(it.peek(), Some(c) if ('\u{20}'..='\u{3f}').contains(c)) {
        it.next();
    }
    if matches!(it.peek(), Some(c) if ('\u{40}'..='\u{7e}').contains(c)) {
        it.next();
    }
}

/// OSC / DCS / SOS / PM / APC body: ends at BEL or ST (ESC \). Stops before a newline so an
/// unterminated sequence cannot swallow the rest of the output.
fn skip_string_body(it: &mut Chars<'_>) {
    loop {
        match it.peek().copied() {
            None | Some('\n') => break,
            Some('\u{7}') => {
                it.next();
                break;
            }
            Some('\u{1b}') => {
                it.next();
                if it.peek() == Some(&'\\') {
                    it.next();
                }
                break;
            }
            Some(_) => {
                it.next();
            }
        }
    }
}

/// `<sha256 hex of raw>.txt`
fn spill_file_name(raw: &[u8]) -> String {
    let digest = Sha256::digest(raw);
    let mut name = String::with_capacity(64 + 4);
    for b in digest {
        let _ = write!(name, "{b:02x}");
    }
    name.push_str(".txt");
    name
}

/// Write the raw bytes to `path` (0600, inside a 0700 directory). Returns whether the full
/// output is on disk. An existing regular file with the content-addressed name is kept.
fn spill(raw: &[u8], dir: &Path, path: &Path) -> bool {
    if DirBuilder::new().recursive(true).mode(0o700).create(dir).is_err() {
        return false;
    }
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => match file.write_all(raw) {
            Ok(()) => true,
            Err(_) => {
                let _ = fs::remove_file(path);
                false
            }
        },
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            fs::symlink_metadata(path).map(|m| m.is_file()).unwrap_or(false)
        }
        Err(_) => false,
    }
}

/// Whether a cut between `text[..i]` and `text[i..]` would split a grapheme cluster: a ZWJ
/// sequence, a variation selector or skin-tone modifier, a combining mark, or a flag pair.
/// This approximates extended grapheme clusters without a segmentation table.
fn splits_cluster(text: &str, i: usize) -> bool {
    if i == 0 || i >= text.len() {
        return false;
    }
    let prev = match text[..i].chars().next_back() {
        Some(c) => c,
        None => return false,
    };
    let next = match text[i..].chars().next() {
        Some(c) => c,
        None => return false,
    };
    prev == '\u{200d}' || next == '\u{200d}' || is_extend(next) || (is_regional(prev) && is_regional(next))
}

fn is_extend(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{036f}'
        | '\u{20d0}'..='\u{20ff}'
        | '\u{fe00}'..='\u{fe0f}'
        | '\u{1f3fb}'..='\u{1f3ff}'
        | '\u{e0020}'..='\u{e007f}')
}

fn is_regional(c: char) -> bool {
    matches!(c, '\u{1f1e6}'..='\u{1f1ff}')
}

/// Largest char boundary <= i.
fn floor_boundary(text: &str, mut i: usize) -> usize {
    if i >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char boundary >= i.
fn ceil_boundary(text: &str, mut i: usize) -> usize {
    if i >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Head of at most `max` bytes, cut at a line end when one is in the second half, and never
/// inside a character or a ZWJ/extend cluster.
fn head_part(text: &str, max: usize) -> &str {
    if max >= text.len() {
        return text;
    }
    let mut end = floor_boundary(text, max);
    while end > 0 && splits_cluster(text, end) {
        end = floor_boundary(text, end - 1);
    }
    let window = &text[..end];
    if let Some(nl) = window.rfind('\n') {
        if nl * 2 >= end {
            return &text[..nl];
        }
    }
    window
}

/// Tail of at most `max` bytes, starting after a line end when one is in the first half of the
/// window, and never inside a character or a ZWJ/extend cluster.
fn tail_part(text: &str, max: usize) -> &str {
    if max == 0 {
        return "";
    }
    if max >= text.len() {
        return text;
    }
    let mut start = ceil_boundary(text, text.len() - max);
    while start < text.len() && splits_cluster(text, start) {
        start = ceil_boundary(text, start + 1);
    }
    let rest = &text[start..];
    if let Some(nl) = rest.find('\n') {
        if nl * 2 <= rest.len() {
            return &text[start + nl + 1..];
        }
    }
    rest
}
