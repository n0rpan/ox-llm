//! C14: OSC sequences: hyperlinks (8), clipboard (52), title, colour queries. Owner: task C14. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/terminal.ts, terminal-colors.ts, terminal-image.ts (hyperlink)

const ESC: char = '\x1b';
const BEL: char = '\x07';
const ST: &str = "\x1b\\";

/// Removes control characters (C0, DEL and C1). Terminals discard OSC payloads that carry them.
fn strip_controls(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// `text` as an OSC 8 hyperlink to `url` (`ESC ] 8 ; ; url ST text ESC ] 8 ; ; ST`). Control
/// characters are removed from `url`.
pub fn hyperlink(text: &str, url: &str) -> String {
    let url = strip_controls(url);
    format!("{ESC}]8;;{url}{ST}{text}{ESC}]8;;{ST}")
}

/// OSC 52 sequence that sets the system clipboard to `text` (base64, `c` selection, ST
/// terminated). Inside tmux (`in_tmux`) the sequence is wrapped in the DCS passthrough.
pub fn osc52_copy(text: &str, in_tmux: bool) -> String {
    use base64::Engine;
    let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = format!("{ESC}]52;c;{payload}{ST}");
    if in_tmux {
        // tmux passthrough: ESC P tmux; <seq with every ESC doubled> ST
        let doubled = seq.replace(ESC, "\x1b\x1b");
        format!("{ESC}Ptmux;{doubled}{ST}")
    } else {
        seq
    }
}

/// The sequence that sets the window title (OSC 0); control characters removed.
pub fn title_sequence(title: &str) -> String {
    format!("{ESC}]0;{}{BEL}", strip_controls(title))
}

/// OSC 10 and OSC 11 colour queries followed by a primary device attributes request (the
/// sentinel that ends the wait for replies).
pub const COLOR_QUERY: &str = "\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c";

/// The OSC 9 desktop notification sequence with `text` (used when a run settles unfocused).
pub fn notify_sequence(text: &str) -> String {
    format!("{ESC}]9;{}{BEL}", strip_controls(text))
}
