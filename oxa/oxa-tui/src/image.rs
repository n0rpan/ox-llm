//! C14: inline images (kitty, iTerm2) and capability detection. Owner: task C14. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/terminal-image.ts
use crate::{ColorMode, ImageProtocol, TermCaps};
use base64::Engine;

fn var<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn has(env: &[(String, String)], key: &str) -> bool {
    var(env, key).is_some_and(|v| !v.is_empty())
}

/// Detect capabilities from the environment (`TERM`, `TERM_PROGRAM`, `COLORTERM`,
/// `KITTY_WINDOW_ID`, `GHOSTTY_RESOURCES_DIR`, `WEZTERM_PANE`, `ITERM_SESSION_ID`, `TMUX`,
/// `NO_COLOR`). Pure: `env` is the complete input. pi's table: kitty,
/// ghostty, wezterm -> kitty images; iTerm2 -> iterm2 images; tmux and screen -> no images;
/// truecolor when `COLORTERM` is `truecolor` or `24bit`.
///
/// `NO_COLOR` has no field in [`TermCaps`]: the theme layer owns the no-colour decision, so it is
/// not consulted here. Inside tmux the hyperlink capability is off (tmux only forwards OSC 8 when
/// it is probed, which a pure function cannot do).
pub fn detect_caps(env: &[(String, String)]) -> TermCaps {
    let term = var(env, "TERM").unwrap_or("").to_ascii_lowercase();
    let term_program = var(env, "TERM_PROGRAM").unwrap_or("").to_ascii_lowercase();
    let colorterm = var(env, "COLORTERM").unwrap_or("").to_ascii_lowercase();
    let truecolor = matches!(colorterm.as_str(), "truecolor" | "24bit") || term.ends_with("-direct");

    let mut images = None;
    let mut hyperlinks = false;
    let mut osc52 = false;

    if has(env, "TMUX") || term.starts_with("tmux") {
        // Image protocols are unreliable under tmux; OSC 52 goes through the DCS passthrough.
        osc52 = true;
    } else if term.starts_with("screen") {
        // screen forwards neither images nor OSC 8.
    } else {
        let kitty_terminal = matches!(
            term_program.as_str(),
            "kitty" | "ghostty" | "wezterm" | "warpterminal" | "rex" | "herdr"
        ) || (term_program == "orca"
            && var(env, "ORCA_IMAGE_PROTOCOL") == Some("kitty"))
            || has(env, "KITTY_WINDOW_ID")
            || term.contains("ghostty")
            || has(env, "GHOSTTY_RESOURCES_DIR")
            || has(env, "WEZTERM_PANE")
            || has(env, "WARP_SESSION_ID")
            || has(env, "WARP_TERMINAL_SESSION_UUID");
        let iterm = has(env, "ITERM_SESSION_ID") || term_program == "iterm.app";
        let alacritty =
            term_program == "alacritty" || term.starts_with("alacritty") || has(env, "ALACRITTY_WINDOW_ID");

        if kitty_terminal {
            images = Some(ImageProtocol::Kitty);
            hyperlinks = true;
            osc52 = true;
        } else if iterm {
            images = Some(ImageProtocol::Iterm2);
            hyperlinks = true;
            osc52 = true;
        } else if has(env, "WT_SESSION") || alacritty || matches!(term_program.as_str(), "vscode" | "zed") {
            hyperlinks = true;
            osc52 = true;
        }
    }

    TermCaps {
        images,
        color: if truecolor {
            ColorMode::TrueColor
        } else {
            ColorMode::Ansi256
        },
        hyperlinks,
        kitty_keyboard: false,
        osc52,
        cell_px: None,
    }
}

/// Pixel dimensions of an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDims {
    pub w: u32,
    pub h: u32,
}

const PNG_SIG: &[u8] = b"\x89PNG\r\n\x1a\n";

fn be_u16(b: &[u8]) -> u32 {
    u16::from_be_bytes([b[0], b[1]]) as u32
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn le_u16(b: &[u8]) -> u32 {
    u16::from_le_bytes([b[0], b[1]]) as u32
}

fn le_u24(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], 0])
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn nonzero(w: u32, h: u32) -> Option<ImageDims> {
    (w > 0 && h > 0).then_some(ImageDims { w, h })
}

fn png_dims(data: &[u8]) -> Option<ImageDims> {
    if data.len() < 24 || !data.starts_with(PNG_SIG) || &data[12..16] != b"IHDR" {
        return None;
    }
    nonzero(be_u32(&data[16..20]), be_u32(&data[20..24]))
}

fn gif_dims(data: &[u8]) -> Option<ImageDims> {
    if data.len() < 10 || !(data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
        return None;
    }
    nonzero(le_u16(&data[6..8]), le_u16(&data[8..10]))
}

fn webp_dims(data: &[u8]) -> Option<ImageDims> {
    if data.get(0..4)? != b"RIFF" || data.get(8..12)? != b"WEBP" {
        return None;
    }
    match data.get(12..16)? {
        // Lossy bitstream: frame tag (3 bytes), start code 9d 01 2a, then 14-bit width and height.
        b"VP8 " => {
            if data.get(23..26)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            nonzero(
                le_u16(data.get(26..28)?) & 0x3fff,
                le_u16(data.get(28..30)?) & 0x3fff,
            )
        }
        // Lossless bitstream: signature 0x2f, then width-1 and height-1 in 14 bits each.
        b"VP8L" => {
            if *data.get(20)? != 0x2f {
                return None;
            }
            let bits = le_u32(data.get(21..25)?);
            nonzero((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
        }
        // Extended: canvas width-1 and height-1 as 24-bit values.
        b"VP8X" => nonzero(le_u24(data.get(24..27)?) + 1, le_u24(data.get(27..30)?) + 1),
        _ => None,
    }
}

fn jpeg_dims(data: &[u8]) -> Option<ImageDims> {
    if !data.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut i = 2;
    while i < data.len() {
        if data[i] != 0xff {
            i += 1;
            continue;
        }
        while i < data.len() && data[i] == 0xff {
            i += 1;
        }
        if i >= data.len() {
            return None;
        }
        let marker = data[i];
        i += 1;
        // Markers without a length: SOI, TEM, RSTn.
        if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        // EOI or start of scan: no frame header found.
        if marker == 0xd9 || marker == 0xda || i + 2 > data.len() {
            return None;
        }
        let seg_len = be_u16(&data[i..i + 2]) as usize;
        if seg_len < 2 {
            return None;
        }
        // Start-of-frame markers, except DHT (C4), JPG (C8) and DAC (CC).
        let is_sof = (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
        if is_sof {
            // Segment layout after the length: precision (1), height (2), width (2).
            if i + 7 > data.len() {
                return None;
            }
            return nonzero(be_u16(&data[i + 5..i + 7]), be_u16(&data[i + 3..i + 5]));
        }
        i += seg_len;
    }
    None
}

/// Dimensions from the file header for PNG, JPEG, GIF and WebP (`mime` = `image/png` ...).
/// `None` for other types or truncated data. Reads only the header bytes.
pub fn image_dims(data: &[u8], mime: &str) -> Option<ImageDims> {
    match mime {
        "image/png" => png_dims(data),
        "image/jpeg" => jpeg_dims(data),
        "image/gif" => gif_dims(data),
        "image/webp" => webp_dims(data),
        _ => None,
    }
}

/// The MIME type by magic bytes (`image/png`, `image/jpeg`, `image/gif`, `image/webp`).
pub fn sniff_image_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(PNG_SIG) {
        Some("image/png")
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The cell box an image occupies: scaled to at most `max_cols` columns keeping the aspect
/// ratio, using `cell_px` (default 9x18 when unknown), at most `max_rows` rows.
/// Images are never enlarged. Returns `(0, 0)` when `max_cols` or `max_rows` is zero.
pub fn image_cell_size(
    dims: ImageDims,
    max_cols: u16,
    max_rows: u16,
    cell_px: Option<(u16, u16)>,
) -> (u16, u16) {
    if max_cols == 0 || max_rows == 0 {
        return (0, 0);
    }
    let (cw, ch) = cell_px.unwrap_or((9, 18));
    let (cw, ch) = (cw.max(1) as f64, ch.max(1) as f64);
    let w = dims.w.max(1) as f64;
    let h = dims.h.max(1) as f64;
    let box_w = max_cols as f64 * cw;
    let box_h = max_rows as f64 * ch;
    // Scale factor that fits the box; 1.0 means the image already fits.
    let scale = (box_w / w).min(box_h / h).min(1.0);
    let cols = ((w * scale) / cw).ceil().clamp(1.0, max_cols as f64) as u16;
    let rows = ((h * scale) / ch).ceil().clamp(1.0, max_rows as f64) as u16;
    (cols, rows)
}

/// The kitty graphics sequence that transmits and displays PNG `data` in `cols` x `rows` cells
/// (`a=T,f=100,q=2`, base64 payload in chunks of 4096 with `m=1` continuation), with image id
/// `id`. Golden-tested.
/// Only PNG payloads (`f=100`) are valid for kitty; `id` 0 omits `i=` (kitty reserves 0).
pub fn encode_kitty(data: &[u8], cols: u16, rows: u16, id: u32) -> String {
    const CHUNK: usize = 4096;
    let payload = base64::engine::general_purpose::STANDARD.encode(data);

    let mut params = String::from("a=T,f=100,q=2");
    if cols > 0 {
        params.push_str(&format!(",c={cols}"));
    }
    if rows > 0 {
        params.push_str(&format!(",r={rows}"));
    }
    if id != 0 {
        params.push_str(&format!(",i={id}"));
    }

    if payload.len() <= CHUNK {
        return format!("\x1b_G{params};{payload}\x1b\\");
    }

    let mut out = String::new();
    let mut chunks = payload.as_bytes().chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none();
        // base64 is ASCII, so each chunk is valid UTF-8.
        let chunk = std::str::from_utf8(chunk).unwrap_or("");
        if first {
            out.push_str(&format!("\x1b_G{params},m=1;{chunk}\x1b\\"));
            first = false;
        } else if last {
            out.push_str(&format!("\x1b_Gm=0;{chunk}\x1b\\"));
        } else {
            out.push_str(&format!("\x1b_Gm=1;{chunk}\x1b\\"));
        }
    }
    out
}

/// The sequence that deletes kitty image `id` (`a=d,d=I`).
pub fn delete_kitty_image(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

/// The iTerm2 inline image sequence (`OSC 1337 ; File=inline=1;width=<cols>;height=<rows>;
/// preserveAspectRatio=1 : <base64> BEL`). Golden-tested.
/// A `name` is sent base64-encoded as `name=` after the fixed parameters.
pub fn encode_iterm2(data: &[u8], cols: u16, rows: u16, name: Option<&str>) -> String {
    let payload = base64::engine::general_purpose::STANDARD.encode(data);
    let mut params = format!("inline=1;width={cols};height={rows};preserveAspectRatio=1");
    if let Some(name) = name {
        let name64 = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
        params.push_str(&format!(";name={name64}"));
    }
    format!("\x1b]1337;File={params}:{payload}\x07")
}

/// Lines that display an image with `protocol`: the first line carries the sequence, followed
/// by `rows - 1` empty lines that reserve the space (pi's layout). The renderer treats a line
/// for which [`is_image_line`] holds as opaque (never truncated or diffed by content).
/// Returns the fallback text ([`image_fallback`]) when the dimensions cannot be read, and for
/// kitty when the payload is not PNG (kitty only accepts PNG with `f=100`).
pub fn render_image(
    data: &[u8],
    mime: &str,
    protocol: ImageProtocol,
    max_cols: u16,
    max_rows: u16,
    cell_px: Option<(u16, u16)>,
    id: u32,
) -> Vec<String> {
    let Some(dims) = image_dims(data, mime) else {
        return vec![image_fallback(mime, None, None)];
    };
    if protocol == ImageProtocol::Kitty && mime != "image/png" {
        return vec![image_fallback(mime, Some(dims), None)];
    }
    let (cols, rows) = image_cell_size(dims, max_cols, max_rows, cell_px);
    if rows == 0 {
        return Vec::new();
    }
    let seq = match protocol {
        ImageProtocol::Kitty => encode_kitty(data, cols, rows, id),
        ImageProtocol::Iterm2 => encode_iterm2(data, cols, rows, None),
    };
    let mut lines = Vec::with_capacity(rows as usize);
    lines.push(seq);
    lines.extend((1..rows).map(|_| String::new()));
    lines
}

/// `true` when `line` contains an inline image sequence.
pub fn is_image_line(line: &str) -> bool {
    line.contains("\x1b_G") || line.contains("\x1b]1337;File=")
}

/// The text shown instead of an image when the terminal has no image protocol or
/// `images.blockImages` is set: `[image <name> <w>x<h>]` (parts omitted when unknown).
/// `mime` is not shown; it only documents what was blocked.
pub fn image_fallback(mime: &str, dims: Option<ImageDims>, name: Option<&str>) -> String {
    let _ = mime;
    let mut out = String::from("[image");
    if let Some(name) = name.filter(|n| !n.is_empty()) {
        out.push(' ');
        out.push_str(name);
    }
    if let Some(d) = dims {
        out.push_str(&format!(" {}x{}", d.w, d.h));
    }
    out.push(']');
    out
}
