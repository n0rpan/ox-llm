//! A12: key and input-event types and the terminal input parser.
// ported from pi packages/tui/src/keys.ts and stdin-buffer.ts
use oxa_proto::KeyId;

/// Kitty event type; legacy terminals only ever produce `Press`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyKind {
    Press,
    Repeat,
    Release,
}

/// One key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// Normalised id (`ctrl+shift+up`, `a`, `enter`, `shift+tab`). For a printable key without
    /// ctrl/alt/super the id is the lowercase base key and `text` holds what to insert. A typed
    /// capital letter is `shift+<letter>` (D-INT), so `shift+l` style bindings match; other
    /// shifted characters (`!`, `?`) keep their own plain id.
    pub id: KeyId,
    /// The text this key inserts (`"A"`, `"é"`, `"😀"`), `None` for non-printing keys and for
    /// chords with ctrl/alt/super.
    pub text: Option<String>,
    pub kind: KeyKind,
}

impl Key {
    /// A press of `id` without text. Panics on an invalid id (tests and constants only).
    pub fn press(id: &str) -> Key {
        Key {
            id: KeyId::parse(id).expect("valid key id"),
            text: None,
            kind: KeyKind::Press,
        }
    }
    /// A press of a printable character (`id` = its lowercase form; a capital letter gets
    /// `shift+`, as the input parser reports it).
    pub fn char(c: char) -> Key {
        let lower: String = c.to_lowercase().collect();
        let name = if c == ' ' {
            "space".to_string()
        } else if is_capital(c) {
            format!("shift+{lower}")
        } else {
            lower
        };
        let id =
            KeyId::parse(&name).unwrap_or_else(|_| KeyId::parse("space").expect("space is a valid key id"));
        Key {
            id,
            text: Some(c.to_string()),
            kind: KeyKind::Press,
        }
    }
    /// `true` when this key's id equals the normalised form of `id`.
    pub fn is(&self, id: &str) -> bool {
        KeyId::parse(id).map(|k| k == self.id).unwrap_or(false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseKind {
    Press,
    Release,
    Move,
    Drag,
    WheelUp,
    WheelDown,
}

/// An SGR (1006) mouse report. `col` and `row` are 0-based screen cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mouse {
    pub kind: MouseKind,
    pub button: MouseButton,
    pub col: u16,
    pub row: u16,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// Everything the terminal can send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    Key(Key),
    /// A complete bracketed paste (markers removed, `\r\n` and `\r` normalised to `\n`).
    Paste(String),
    Mouse(Mouse),
    Resize {
        cols: u16,
        rows: u16,
    },
    FocusGained,
    FocusLost,
    /// Reply to an OSC query: `code` = 10, 11, 52..; `data` = the payload after
    /// `<code>;` without the terminator.
    OscReply {
        code: u16,
        data: String,
    },
    /// `CSI ? <flags> u`: the kitty keyboard protocol is active with these flags.
    KittyFlags(u32),
    /// Primary device attributes reply (`CSI ? .. c`), used as the end-of-queries sentinel.
    DeviceAttributes(String),
    /// `CSI 6 ; <h> ; <w> t`: cell size in pixels.
    CellSize {
        w: u16,
        h: u16,
    },
    /// An application program command from the terminal (`ESC _ G <body> ESC \`, the reply
    /// to a kitty graphics query): the body with its leading `G`.
    Apc(String),
}

/// Time to wait after a lone `ESC` (or an unfinished sequence) before reporting it as the
/// Escape key (pi's local default).
pub const ESC_TIMEOUT_MS: u64 = 10;

/// Parse as many complete events as `buf` holds. Returns the events and the number of bytes
/// consumed; the caller keeps `buf[consumed..]` and calls again when more bytes arrive.
///
/// * Never panics, for any bytes. Invalid UTF-8 is skipped byte by byte.
/// * An incomplete sequence at the end (a lone `ESC`, a partial CSI/OSC, a partial UTF-8
///   character, an unterminated bracketed paste) is **not** consumed.
/// * Understands: the kitty keyboard protocol (`CSI u` with modifiers, event types, alternate
///   keys and associated text), xterm `modifyOtherKeys`, the legacy sequence table (arrows,
///   home/end, F1-F12, `ESC` + char = alt, C0 controls = ctrl+letter, `0x7f` = backspace,
///   `ESC [ Z` = shift+tab), bracketed paste (`CSI 200~ .. CSI 201~`), SGR mouse (`CSI < ..
///   M/m`), focus (`CSI I` / `CSI O`), OSC replies (BEL or ST terminated), `CSI ? .. u`,
///   `CSI ? .. c`, `CSI 6 ; h ; w t`.
/// * Splitting the input at any byte offset and feeding the pieces in order (re-offering the
///   unconsumed rest) yields the same events as feeding it whole.
///
/// Acceptance (plan A12): key-id parser; kitty protocol + legacy sequences table; ESC timeout.
pub fn parse_input(buf: &[u8]) -> (Vec<InputEvent>, usize) {
    scan(buf, false)
}

/// The ESC timeout fired: interpret the pending bytes as final. A lone `ESC` is the Escape
/// key; `ESC` + bytes that never completed a sequence are reported as alt+key / literal keys.
/// Consumes all of `buf`. An unterminated paste is delivered as a `Paste` of what arrived.
pub fn flush_pending(buf: &[u8]) -> Vec<InputEvent> {
    scan(buf, true).0
}

/// `true` when a terminal without the kitty keyboard protocol can send `id` as itself: its
/// legacy bytes ([`encode_legacy`]) parse back to `id`. `ctrl+shift+f` cannot (it arrives as
/// ctrl+f), nor can `shift+enter` (CR, enter); `alt+s`, `ctrl+up` and `shift+tab` can.
pub fn legacy_sendable(id: &KeyId) -> bool {
    let Some(bytes) = encode_legacy(id) else {
        return false;
    };
    matches!(flush_pending(&bytes).as_slice(), [InputEvent::Key(k)] if &k.id == id)
}

/// The legacy byte sequence a terminal without the kitty protocol sends for `id`, if it has
/// one (used by tests and by `TestTerminal` scripts): `encode_legacy("ctrl+c") == Some(b"\x03")`.
pub fn encode_legacy(id: &KeyId) -> Option<Vec<u8>> {
    if id.has_mod("super") {
        return None;
    }
    let ctrl = id.has_mod("ctrl");
    let shift = id.has_mod("shift");
    let alt = id.has_mod("alt");
    let name = id.key();
    // xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl
    let m = 1 + u32::from(shift) + 2 * u32::from(alt) + 4 * u32::from(ctrl);
    // Named keys with a CSI form carry alt inside the modifier parameter, not as an ESC prefix.
    let csi_named = matches!(
        name,
        "up" | "down"
            | "left"
            | "right"
            | "home"
            | "end"
            | "insert"
            | "delete"
            | "pageup"
            | "pagedown"
            | "f1"
            | "f2"
            | "f3"
            | "f4"
            | "f5"
            | "f6"
            | "f7"
            | "f8"
            | "f9"
            | "f10"
            | "f11"
            | "f12"
    );
    let base: Vec<u8> = match name {
        "up" => csi_letter(b'A', m),
        "down" => csi_letter(b'B', m),
        "right" => csi_letter(b'C', m),
        "left" => csi_letter(b'D', m),
        "home" => csi_letter(b'H', m),
        "end" => csi_letter(b'F', m),
        "f1" | "f2" | "f3" | "f4" => {
            let letter = b'P' + (name.as_bytes()[1] - b'1');
            if m == 1 {
                vec![ESC, b'O', letter]
            } else {
                csi_letter(letter, m)
            }
        }
        "insert" => csi_tilde(2, m),
        "delete" => csi_tilde(3, m),
        "pageup" => csi_tilde(5, m),
        "pagedown" => csi_tilde(6, m),
        "f5" => csi_tilde(15, m),
        "f6" => csi_tilde(17, m),
        "f7" => csi_tilde(18, m),
        "f8" => csi_tilde(19, m),
        "f9" => csi_tilde(20, m),
        "f10" => csi_tilde(21, m),
        "f11" => csi_tilde(23, m),
        "f12" => csi_tilde(24, m),
        "enter" => {
            if ctrl || shift {
                return None;
            }
            b"\r".to_vec()
        }
        "tab" => {
            if ctrl {
                return None;
            }
            if shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        "backspace" => {
            if ctrl {
                vec![0x08]
            } else {
                vec![0x7f]
            }
        }
        "escape" => {
            if ctrl || shift {
                return None;
            }
            vec![ESC]
        }
        "space" => {
            if ctrl {
                vec![0x00]
            } else {
                vec![b' ']
            }
        }
        _ => {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            if c.is_ascii_alphabetic() {
                if ctrl {
                    vec![c.to_ascii_lowercase() as u8 - b'a' + 1]
                } else if shift {
                    vec![c.to_ascii_uppercase() as u8]
                } else {
                    vec![c as u8]
                }
            } else if ctrl {
                vec![match c {
                    '[' => 0x1b,
                    '\\' => 0x1c,
                    ']' => 0x1d,
                    '^' => 0x1e,
                    '_' | '-' => 0x1f,
                    _ => return None,
                }]
            } else if shift {
                return None;
            } else {
                c.to_string().into_bytes()
            }
        }
    };
    if alt && !csi_named {
        let mut out = vec![ESC];
        out.extend(base);
        Some(out)
    } else {
        Some(base)
    }
}

// ---------------------------------------------------------------------------------------------
// Internals. Everything below is private; the public surface is the frozen list above.
// ---------------------------------------------------------------------------------------------

const ESC: u8 = 0x1b;
const M_SHIFT: u8 = 1;
const M_ALT: u8 = 2;
const M_CTRL: u8 = 4;
const M_SUPER: u8 = 8;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// One parser step: the events it produced and the index after the bytes it used, or `None`
/// when the bytes at `i` are an incomplete sequence (more input could still complete it).
type Step = Option<(Vec<InputEvent>, usize)>;

fn scan(buf: &[u8], flush: bool) -> (Vec<InputEvent>, usize) {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let (events, next) = match step(buf, i) {
            Some(r) => r,
            None if !flush => break,
            None => fallback(buf, i),
        };
        out.extend(events);
        // Progress is guaranteed by construction; the guard keeps a bug from looping forever.
        i = next.max(i + 1);
    }
    (out, i)
}

fn step(buf: &[u8], i: usize) -> Step {
    if buf[i] != ESC {
        return match decode_char(buf, i) {
            Decoded::Char(c, n) => Some((one(char_key(c, 0)), i + n)),
            Decoded::Invalid => Some((Vec::new(), i + 1)),
            Decoded::Incomplete => None,
        };
    }
    let next = *buf.get(i + 1)?;
    match next {
        b'[' => csi_step(buf, i),
        b']' => osc_step(buf, i),
        // `ESC _ G`: a kitty graphics reply (alt+_ followed at once by `G` does not occur).
        b'_' if buf.get(i + 2) == Some(&b'G') => apc_step(buf, i),
        b'O' => {
            let x = *buf.get(i + 2)?;
            let name = match x {
                b'A' => "up",
                b'B' => "down",
                b'C' => "right",
                b'D' => "left",
                b'H' => "home",
                b'F' => "end",
                b'P' => "f1",
                b'Q' => "f2",
                b'R' => "f3",
                b'S' => "f4",
                // `ESC O` followed by anything else is alt+O, then the byte is parsed normally.
                _ => return Some((one(char_key('O', M_ALT)), i + 2)),
            };
            Some((one(mk(0, name, None, KeyKind::Press)), i + 3))
        }
        ESC => Some((esc_event(), i + 1)),
        _ => match decode_char(buf, i + 1) {
            Decoded::Char(c, n) => Some((one(char_key(c, M_ALT)), i + 1 + n)),
            Decoded::Invalid => Some((esc_event(), i + 1)),
            Decoded::Incomplete => None,
        },
    }
}

/// Used by the flush path only, for bytes that `step` cannot complete.
fn fallback(buf: &[u8], i: usize) -> (Vec<InputEvent>, usize) {
    if buf[i] != ESC {
        // A truncated UTF-8 character at the end: skip its bytes one by one.
        return (Vec::new(), i + 1);
    }
    let len = buf.len();
    match buf.get(i + 1) {
        None => (esc_event(), len),
        Some(&b'[') if buf[i..].starts_with(PASTE_START) => {
            let content = &buf[i + PASTE_START.len()..];
            (vec![InputEvent::Paste(normalise_paste(content))], len)
        }
        // An unfinished OSC reply is noise; drop it.
        Some(&b']') => (Vec::new(), len),
        // So is an unfinished graphics reply.
        Some(&b'_') if buf.get(i + 2) == Some(&b'G') => (Vec::new(), len),
        Some(_) => match decode_char(buf, i + 1) {
            Decoded::Char(c, n) => (one(char_key(c, M_ALT)), i + 1 + n),
            _ => (esc_event(), i + 1),
        },
    }
}

fn one(key: Option<Key>) -> Vec<InputEvent> {
    key.map(InputEvent::Key).into_iter().collect()
}

fn esc_event() -> Vec<InputEvent> {
    one(mk(0, "escape", None, KeyKind::Press))
}

enum Decoded {
    Char(char, usize),
    Invalid,
    Incomplete,
}

fn decode_char(buf: &[u8], i: usize) -> Decoded {
    let b = buf[i];
    if b < 0x80 {
        return Decoded::Char(b as char, 1);
    }
    let n = match b {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return Decoded::Invalid,
    };
    let end = (i + n).min(buf.len());
    match std::str::from_utf8(&buf[i..end]) {
        Ok(s) => match s.chars().next() {
            Some(c) if end - i == n => Decoded::Char(c, n),
            _ => Decoded::Invalid,
        },
        Err(e) if e.error_len().is_none() => Decoded::Incomplete,
        Err(_) => Decoded::Invalid,
    }
}

fn csi_step(buf: &[u8], i: usize) -> Step {
    if buf[i..].starts_with(PASTE_START) {
        return paste_step(buf, i);
    }
    let mut j = i + 2;
    while j < buf.len() && (0x20..=0x3f).contains(&buf[j]) {
        j += 1;
    }
    let fin = *buf.get(j)?;
    if !(0x40..=0x7e).contains(&fin) {
        // Malformed: drop the `ESC [` introducer and let the next byte parse on its own.
        return Some((Vec::new(), i + 2));
    }
    Some((decode_csi(&buf[i + 2..j], fin), j + 1))
}

fn paste_step(buf: &[u8], i: usize) -> Step {
    let start = i + PASTE_START.len();
    let pos = find(&buf[start..], PASTE_END)?;
    let content = &buf[start..start + pos];
    Some((
        vec![InputEvent::Paste(normalise_paste(content))],
        start + pos + PASTE_END.len(),
    ))
}

fn normalise_paste(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn osc_step(buf: &[u8], i: usize) -> Step {
    let mut j = i + 2;
    loop {
        match *buf.get(j)? {
            0x07 => return Some((osc_event(&buf[i + 2..j]), j + 1)),
            ESC => match buf.get(j + 1) {
                None => return None,
                Some(&b'\\') => return Some((osc_event(&buf[i + 2..j]), j + 2)),
                // ESC that does not start an ST terminator: the OSC is broken, drop it.
                Some(_) => return Some((Vec::new(), j)),
            },
            _ => j += 1,
        }
    }
}

/// `ESC _ <body> ESC \`: the body as one event. Unterminated: wait for more.
fn apc_step(buf: &[u8], i: usize) -> Step {
    let end = find(&buf[i + 2..], b"\x1b\\")?;
    let body = String::from_utf8_lossy(&buf[i + 2..i + 2 + end]).into_owned();
    Some((vec![InputEvent::Apc(body)], i + 2 + end + 2))
}

fn osc_event(body: &[u8]) -> Vec<InputEvent> {
    let (code, data) = match body.iter().position(|&b| b == b';') {
        Some(p) => (&body[..p], &body[p + 1..]),
        None => (body, &b""[..]),
    };
    let Some(code) = std::str::from_utf8(code).ok().and_then(|s| s.parse::<u16>().ok()) else {
        return Vec::new();
    };
    vec![InputEvent::OscReply {
        code,
        data: String::from_utf8_lossy(data).into_owned(),
    }]
}

fn num(s: &str) -> Option<u32> {
    s.parse().ok()
}

fn to_u16(v: u32) -> u16 {
    v.min(u16::MAX as u32) as u16
}

/// Modifier bits and event kind from field `idx` of a CSI parameter list (`1;5:3` = ctrl,
/// release). Bits use the kitty layout: shift 1, alt 2, ctrl 4, super 8.
fn mods_kind(fields: &[&str], idx: usize) -> (u8, KeyKind) {
    let sub: Vec<&str> = fields
        .get(idx)
        .map(|f| f.split(':').collect())
        .unwrap_or_default();
    let m = sub.first().and_then(|s| num(s)).unwrap_or(1);
    let mods = (m.saturating_sub(1) & 0x0f) as u8;
    let kind = match sub.get(1).and_then(|s| num(s)) {
        Some(2) => KeyKind::Repeat,
        Some(3) => KeyKind::Release,
        _ => KeyKind::Press,
    };
    (mods, kind)
}

fn decode_csi(seq: &[u8], fin: u8) -> Vec<InputEvent> {
    let (marker, body) = match seq.first() {
        Some(&m) if matches!(m, b'<' | b'=' | b'>' | b'?') => (Some(m), &seq[1..]),
        _ => (None, seq),
    };
    let body = std::str::from_utf8(body).unwrap_or("");
    let fields: Vec<&str> = if body.is_empty() {
        Vec::new()
    } else {
        body.split(';').collect()
    };
    match marker {
        Some(b'<') if fin == b'M' || fin == b'm' => mouse_event(&fields, fin)
            .map(InputEvent::Mouse)
            .into_iter()
            .collect(),
        Some(b'?') => match fin {
            b'u' => vec![InputEvent::KittyFlags(
                fields.first().and_then(|s| num(s)).unwrap_or(0),
            )],
            b'c' => vec![InputEvent::DeviceAttributes(body.to_string())],
            _ => Vec::new(),
        },
        Some(_) => Vec::new(),
        None => plain_csi(&fields, fin),
    }
}

fn letter_key(fin: u8) -> Option<&'static str> {
    Some(match fin {
        b'A' => "up",
        b'B' => "down",
        b'C' => "right",
        b'D' => "left",
        b'H' => "home",
        b'F' => "end",
        b'P' => "f1",
        b'Q' => "f2",
        b'R' => "f3",
        b'S' => "f4",
        _ => return None,
    })
}

fn plain_csi(fields: &[&str], fin: u8) -> Vec<InputEvent> {
    if let Some(name) = letter_key(fin) {
        // `CSI row ; col R` is a cursor position report, not F3. Oxa never asks for one.
        if fin == b'R' && fields.first().is_some_and(|f| *f != "1") {
            return Vec::new();
        }
        let (mods, kind) = mods_kind(fields, 1);
        return one(mk(mods, name, None, kind));
    }
    match fin {
        b'u' => one(kitty_csi_u(fields)),
        b'~' => one(tilde_csi(fields)),
        b'Z' => one(mk(M_SHIFT, "tab", None, KeyKind::Press)),
        b'I' if fields.is_empty() => vec![InputEvent::FocusGained],
        b'O' if fields.is_empty() => vec![InputEvent::FocusLost],
        b't' if fields.len() == 3 && fields[0] == "6" => vec![InputEvent::CellSize {
            w: to_u16(num(fields[2]).unwrap_or(0)),
            h: to_u16(num(fields[1]).unwrap_or(0)),
        }],
        _ => Vec::new(),
    }
}

/// `CSI n ; mods ~`: the editing and function keys, and xterm `modifyOtherKeys` (`CSI 27 ; m ; c ~`).
fn tilde_csi(fields: &[&str]) -> Option<Key> {
    let n = num(fields.first()?)?;
    let (mods, kind) = mods_kind(fields, 1);
    if n == 27 {
        let cp = num(fields.get(2)?)?;
        return key_from_codepoint(cp, None, None, mods, kind, None);
    }
    let name: String = match n {
        1 | 7 => "home".into(),
        2 => "insert".into(),
        3 => "delete".into(),
        4 | 8 => "end".into(),
        5 => "pageup".into(),
        6 => "pagedown".into(),
        11..=15 => format!("f{}", n - 10),
        17..=21 => format!("f{}", n - 11),
        23 | 24 => format!("f{}", n - 12),
        _ => return None,
    };
    mk(mods, &name, None, kind)
}

/// `CSI code[:shifted[:base]] ; mods[:event] ; text[:text] u`: the kitty keyboard protocol.
fn kitty_csi_u(fields: &[&str]) -> Option<Key> {
    let mut cps = fields.first()?.split(':');
    let cp = num(cps.next()?)?;
    let shifted = cps.next().and_then(num).filter(|&s| s != 0);
    let base = cps.next().and_then(num).filter(|&b| b != 0);
    let (mods, kind) = mods_kind(fields, 1);
    let text = fields
        .get(2)
        .map(|t| {
            t.split(':')
                .filter_map(num)
                .filter_map(char::from_u32)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty());
    key_from_codepoint(cp, shifted, base, mods, kind, text)
}

/// Functional keys of the kitty protocol that have a name.
fn kitty_named(code: u32) -> Option<String> {
    let name = match code {
        27 => "escape",
        13 | 57414 => "enter",
        9 => "tab",
        127 => "backspace",
        57417 => "left",
        57418 => "right",
        57419 => "up",
        57420 => "down",
        57421 => "pageup",
        57422 => "pagedown",
        57423 => "home",
        57424 => "end",
        57425 => "insert",
        57426 => "delete",
        57364..=57398 => return Some(format!("f{}", code - 57363)),
        _ => return None,
    };
    Some(name.to_string())
}

/// Keypad keys that insert a character (the kitty code points 57399..).
fn keypad_char(code: u32) -> Option<char> {
    match code {
        57399..=57408 => char::from_u32(u32::from(b'0') + (code - 57399)),
        57409 => Some('.'),
        57410 => Some('/'),
        57411 => Some('*'),
        57412 => Some('-'),
        57413 => Some('+'),
        57415 => Some('='),
        _ => None,
    }
}

/// A key from a kitty or xterm code point. `base` (the layout-independent key) wins for the
/// id when present; `shifted` is the character with shift applied, used when there is no text.
fn key_from_codepoint(
    cp: u32,
    shifted: Option<u32>,
    base: Option<u32>,
    mods: u8,
    kind: KeyKind,
    text: Option<String>,
) -> Option<Key> {
    let code = base.unwrap_or(cp);
    if let Some(name) = kitty_named(code) {
        return mk(mods, &name, None, kind);
    }
    let printed = keypad_char(cp).or_else(|| char::from_u32(cp))?;
    let key_char = keypad_char(code).or_else(|| char::from_u32(code))?;
    if printed.is_control() || key_char.is_control() {
        return None;
    }
    let has_chord = mods & (M_CTRL | M_ALT | M_SUPER) != 0;
    let text: Option<String> = if kind == KeyKind::Release || has_chord {
        None
    } else if let Some(t) = text {
        Some(t)
    } else if mods & M_SHIFT != 0 {
        shifted.and_then(char::from_u32).map(|c| c.to_string())
    } else {
        Some(printed.to_string())
    };
    // The text carries shift, so the id is the plain base key; only a capital letter keeps
    // `shift+` (so `shift+l` bindings match what was typed).
    let name: String = key_char.to_lowercase().collect();
    let id_mods = match &text {
        Some(t) if capital_of(t, &name) => M_SHIFT,
        Some(_) => 0,
        None => mods,
    };
    mk(id_mods, &name, text, kind)
}

/// SGR mouse: `CSI < b ; x ; y M` (press/motion) or `m` (release).
fn mouse_event(fields: &[&str], fin: u8) -> Option<Mouse> {
    if fields.len() < 3 {
        return None;
    }
    let b = num(fields[0])?;
    let col = num(fields[1])?.saturating_sub(1);
    let row = num(fields[2])?.saturating_sub(1);
    let low = b & 3;
    let button = match low {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        _ => MouseButton::None,
    };
    let (kind, button) = if (b & 64) != 0 {
        match low {
            0 => (MouseKind::WheelUp, MouseButton::None),
            1 => (MouseKind::WheelDown, MouseButton::None),
            _ => return None,
        }
    } else if (b & 32) != 0 {
        if low == 3 {
            (MouseKind::Move, MouseButton::None)
        } else {
            (MouseKind::Drag, button)
        }
    } else if fin == b'm' {
        (MouseKind::Release, button)
    } else {
        (MouseKind::Press, button)
    };
    Some(Mouse {
        kind,
        button,
        col: to_u16(col),
        row: to_u16(row),
        shift: (b & 4) != 0,
        alt: (b & 8) != 0,
        ctrl: (b & 16) != 0,
    })
}

/// A key from a character typed as a single byte or UTF-8 sequence, with modifiers applied.
fn char_key(c: char, mods: u8) -> Option<Key> {
    let (name, mods, text): (String, u8, Option<String>) = match c {
        '\r' => ("enter".into(), mods, None),
        // LF is what a legacy terminal sends for ctrl+j (Enter sends CR); pi's editor inserts a
        // new line for it (packages/tui/src/components/editor.ts, `data === "\n"`).
        '\n' => ("j".into(), mods | M_CTRL, None),
        '\t' => ("tab".into(), mods, None),
        // 0x08 and 0x7f are both the backspace key on legacy terminals.
        '\u{8}' | '\u{7f}' => ("backspace".into(), mods, None),
        '\0' => ("space".into(), mods | M_CTRL, None),
        '\u{1}'..='\u{1a}' => {
            let letter = (c as u8 - 1 + b'a') as char;
            (letter.to_string(), mods | M_CTRL, None)
        }
        '\u{1c}'..='\u{1f}' => {
            // 0x1f is ctrl+- (pi keys.ts: `if (data === "\x1f") return "ctrl+-"`).
            let sym = ["\\", "]", "^", "-"][(c as u8 - 0x1c) as usize];
            (sym.to_string(), mods | M_CTRL, None)
        }
        ' ' => {
            let text = if mods & (M_CTRL | M_ALT | M_SUPER) == 0 {
                Some(" ".to_string())
            } else {
                None
            };
            ("space".into(), mods, text)
        }
        _ => {
            let name: String = c.to_lowercase().collect();
            let text = if mods & (M_CTRL | M_ALT | M_SUPER) == 0 {
                Some(c.to_string())
            } else {
                None
            };
            // A capital letter was typed with shift (or caps lock): the id says so.
            let mods = if is_capital(c) { mods | M_SHIFT } else { mods };
            (name, mods, text)
        }
    };
    mk(mods, &name, text, KeyKind::Press)
}

/// `true` for a letter that has a different lowercase form (`L`, `É`), i.e. one typed with shift.
fn is_capital(c: char) -> bool {
    c.is_uppercase() && c.to_lowercase().next().is_some_and(|l| l != c)
}

/// `true` when `text` is the single capital letter of the key `name`.
fn capital_of(text: &str, name: &str) -> bool {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => is_capital(c) && c.to_lowercase().collect::<String>() == name,
        _ => false,
    }
}

/// Build a key from modifier bits and a name, normalising the id through `KeyId::parse`.
fn mk(mods: u8, name: &str, text: Option<String>, kind: KeyKind) -> Option<Key> {
    let mut s = String::new();
    if mods & M_CTRL != 0 {
        s.push_str("ctrl+");
    }
    if mods & M_SHIFT != 0 {
        s.push_str("shift+");
    }
    if mods & M_ALT != 0 {
        s.push_str("alt+");
    }
    if mods & M_SUPER != 0 {
        s.push_str("super+");
    }
    s.push_str(name);
    let id = KeyId::parse(&s).ok()?;
    Some(Key { id, text, kind })
}

fn csi_letter(letter: u8, m: u32) -> Vec<u8> {
    if m == 1 {
        vec![ESC, b'[', letter]
    } else {
        format!("\x1b[1;{m}{}", letter as char).into_bytes()
    }
}

fn csi_tilde(n: u32, m: u32) -> Vec<u8> {
    if m == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{m}~").into_bytes()
    }
}
