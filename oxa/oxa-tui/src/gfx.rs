//! Kitty graphics for the animated scene of OXA's activity line (ARCADE-ANIMATION.md, the
//! graphics tier): the capability query, one transmission per image and Unicode placeholders
//! to show it. Pure encoders and parsers; the terminal I/O is the caller's.
//!
//! How an image gets on screen without a placement per frame: every image is transmitted
//! ONCE with a virtual placement (`a=T,U=1`, `c` x `r` cells). A row of the placeholder
//! character U+10EEEE whose foreground colour encodes the image id then shows it. Switching
//! the frame of an animation is rewriting that row with another colour: a few bytes of text,
//! drawn by the ordinary line renderer inside its synchronized update (no flicker, nothing
//! left behind when the row is erased or scrolls away).
use crate::{Color, InputEvent, Terminal};
use base64::Engine;

/// The id of the capability query (any id works; the reply echoes it).
pub const GFX_QUERY_ID: u32 = 31;

/// The standard capability query: a 1x1 RGB image "sent" with `a=q` (the terminal checks it
/// and stores nothing). A terminal with the protocol answers `ESC _G i=31;OK ESC \`; one
/// without it stays silent, so the caller sends a request every terminal answers (primary
/// device attributes) right after it and stops waiting at that reply or at its timeout.
pub fn gfx_query() -> String {
    format!("\x1b_Gi={GFX_QUERY_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\")
}

/// `Some(true)` for the query's `OK` reply, `Some(false)` for an error reply to it, `None` for
/// any other APC body (`body` = the text between `ESC _` and `ESC \`).
pub fn parse_gfx_reply(body: &str) -> Option<bool> {
    let rest = body.strip_prefix('G')?;
    let (keys, status) = rest.split_once(';')?;
    keys.split(',')
        .any(|k| k == format!("i={GFX_QUERY_ID}"))
        .then_some(status == "OK")
}

/// The answer to the query among start-up events (`None` = no reply: no graphics).
pub fn gfx_reply_in(events: &[InputEvent]) -> Option<bool> {
    events.iter().find_map(|e| match e {
        InputEvent::Apc(body) => parse_gfx_reply(body),
        _ => None,
    })
}

/// `true` for terminals known to draw Unicode placeholders (kitty, Ghostty), from the
/// environment. The query alone is not enough: some terminals implement the protocol without
/// placeholders and would show the placeholder character itself. Not under tmux or screen.
pub fn env_gfx_placeholders(env: &[(String, String)]) -> bool {
    let get = |k: &str| {
        env.iter()
            .find(|(n, v)| n == k && !v.is_empty())
            .map(|(_, v)| v.to_ascii_lowercase())
    };
    let term = get("TERM").unwrap_or_default();
    if get("TMUX").is_some() || term.starts_with("tmux") || term.starts_with("screen") {
        return false;
    }
    let program = get("TERM_PROGRAM").unwrap_or_default();
    term.contains("kitty")
        || term.contains("ghostty")
        || program == "kitty"
        || program == "ghostty"
        || get("KITTY_WINDOW_ID").is_some()
        || get("GHOSTTY_RESOURCES_DIR").is_some()
}

/// Transmit the RGBA image `rgba` (`w` x `h` pixels, 4 bytes each) as image `id` with a
/// virtual placement of `cols` x `rows` cells: `a=T,U=1,f=32,q=2`, base64 in chunks of 4096
/// (`m=1` on all but the last). Nothing is drawn by this; see [`gfx_placeholder_row`].
/// Empty when the data does not match the size or `id` is 0.
pub fn gfx_transmit(id: u32, w: u32, h: u32, rgba: &[u8], cols: u16, rows: u16) -> String {
    if id == 0 || rgba.len() != (w as usize) * (h as usize) * 4 || rgba.is_empty() {
        return String::new();
    }
    const CHUNK: usize = 4096;
    let payload = base64::engine::general_purpose::STANDARD.encode(rgba);
    let head = format!("a=T,U=1,f=32,q=2,i={id},s={w},v={h},c={cols},r={rows}");
    let mut out = String::with_capacity(payload.len() + payload.len() / CHUNK * 12 + 64);
    let chunks: Vec<&str> = payload
        .as_bytes()
        .chunks(CHUNK)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    for (n, chunk) in chunks.iter().enumerate() {
        let more = u8::from(n + 1 < chunks.len());
        if n == 0 {
            out.push_str(&format!("\x1b_G{head},m={more};{chunk}\x1b\\"));
        } else {
            out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
        }
    }
    out
}

/// The placeholder character of the kitty graphics protocol.
pub const GFX_PLACEHOLDER: char = '\u{10EEEE}';

/// One row of `cols` cells that shows row 0 of image `id` (ids up to 24 bits): the id as a
/// 24-bit foreground colour, then the placeholder once per cell (the cells after the first
/// continue the row by the protocol's rule), then a foreground reset. `cols` columns wide.
pub fn gfx_placeholder_row(id: u32, cols: u16) -> String {
    let (r, g, b) = ((id >> 16) & 0xff, (id >> 8) & 0xff, id & 0xff);
    let mut out = format!("\x1b[38;2;{r};{g};{b}m");
    for _ in 0..cols {
        out.push(GFX_PLACEHOLDER);
    }
    out.push_str("\x1b[39m");
    out
}

/// Delete image `id` and its data (`a=d,d=I`): its placeholders show nothing afterwards.
pub fn gfx_delete(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

/// A theme colour as RGB (`None` for the terminal's default colour).
pub fn color_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Default => None,
        Color::Indexed(n) => Some(crate::theme::indexed_rgb(n)),
        Color::Rgb(r, g, b) => Some((r, g, b)),
    }
}

/// `true` when the terminal answered the graphics query at start ([`Terminal::graphics`])
/// and is one that draws placeholders.
pub fn gfx_available(term: &dyn Terminal, env: &[(String, String)]) -> bool {
    term.graphics() && env_gfx_placeholders(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_reply() {
        assert_eq!(gfx_query(), "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
        assert_eq!(parse_gfx_reply("Gi=31;OK"), Some(true));
        assert_eq!(parse_gfx_reply("Gi=31;ENOTSUPPORTED:nope"), Some(false));
        assert_eq!(parse_gfx_reply("Gi=7;OK"), None);
        assert_eq!(parse_gfx_reply("other"), None);
    }
}
