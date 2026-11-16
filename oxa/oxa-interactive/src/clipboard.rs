//! D12: clipboard copy and paste (text, images, files).
// ported from pi packages/coding-agent/src/utils/clipboard.ts and clipboard-image.ts
use oxa_proto::{codes, OxaError, Part};
use oxa_tui::TermCaps;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// How the clipboard is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipMethod {
    /// `wl-copy` / `wl-paste` (when `WAYLAND_DISPLAY` is set and the tools exist).
    Wayland,
    /// `xclip` (when `DISPLAY` is set and the tool exists).
    Xclip,
    /// OSC 52 through the terminal (copy only; also the choice over SSH).
    Osc52,
    None,
}

/// Choose the copy method. Pure given its inputs: `has_tool` answers whether a program is on
/// `PATH`. Order: Wayland, Xclip, Osc52 (when `caps.osc52` or over SSH), None.
pub fn choose_clip_method(
    env: &[(String, String)],
    caps: &TermCaps,
    has_tool: &dyn Fn(&str) -> bool,
) -> ClipMethod {
    let var = |key: &str| {
        env.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    };
    if var("WAYLAND_DISPLAY").is_some() && has_tool("wl-copy") {
        return ClipMethod::Wayland;
    }
    if var("DISPLAY").is_some() && has_tool("xclip") {
        return ClipMethod::Xclip;
    }
    let over_ssh = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY", "MOSH_CONNECTION"]
        .iter()
        .any(|k| var(k).is_some());
    // pi utils/clipboard.ts: without a display the terminal is the only clipboard route
    // (containers, a bare console), so OSC 52 is emitted; with a display and no tool the copy
    // fails instead (OSC 52 cannot be verified).
    let headless = var("WAYLAND_DISPLAY").is_none() && var("DISPLAY").is_none();
    if caps.osc52 || over_ssh || headless || var("OXA_CLIPBOARD_OSC52") == Some("1") {
        return ClipMethod::Osc52;
    }
    ClipMethod::None
}

/// Copy `text`. For `Osc52` returns the sequence the caller must write to the terminal;
/// for the tool methods runs the tool (text on stdin) and returns `None`. Errors: `io` when
/// the tool fails, `not_found` for `ClipMethod::None`.
pub fn copy_text(text: &str, method: &ClipMethod, in_tmux: bool) -> Result<Option<String>, OxaError> {
    let (program, args): (&str, &[&str]) = match method {
        ClipMethod::Osc52 => return Ok(Some(oxa_tui::osc52_copy(text, in_tmux))),
        ClipMethod::None => {
            return Err(OxaError::new(
                codes::NOT_FOUND,
                "no clipboard method is available",
            ))
        }
        ClipMethod::Wayland => ("wl-copy", &[]),
        ClipMethod::Xclip => ("xclip", &["-selection", "clipboard"]),
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        // The clipboard tools fork a helper that keeps serving the selection; it must not
        // hold our output pipes open.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| OxaError::io(&format!("start {program}"), &e))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| OxaError::io(&format!("write the clipboard text to {program}"), &e))?;
    }
    let status = child
        .wait()
        .map_err(|e| OxaError::io(&format!("wait for {program}"), &e))?;
    if !status.success() {
        return Err(OxaError::new(codes::IO, format!("{program} failed ({status})")));
    }
    Ok(None)
}

/// An image read from the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipImage {
    pub data: Vec<u8>,
    pub mime: String,
}

/// Read an image from the clipboard (`wl-paste --type image/png` or `xclip -t image/png -o`),
/// `Ok(None)` when the clipboard holds no image or there is no method.
pub fn read_clipboard_image(method: &ClipMethod) -> Result<Option<ClipImage>, OxaError> {
    let (program, args): (&str, &[&str]) = match method {
        ClipMethod::Wayland => ("wl-paste", &["--no-newline", "--type", "image/png"]),
        ClipMethod::Xclip => ("xclip", &["-selection", "clipboard", "-t", "image/png", "-o"]),
        ClipMethod::Osc52 | ClipMethod::None => return Ok(None),
    };
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| OxaError::io(&format!("start {program}"), &e))?;
    // A non-zero exit is how both tools report "no image on the clipboard".
    if !out.status.success() || out.stdout.is_empty() {
        return Ok(None);
    }
    Ok(Some(ClipImage {
        data: out.stdout,
        mime: "image/png".to_string(),
    }))
}

/// Save a pasted image under `dir` as `paste-<sha256 prefix>.<ext>` (0600) and return the path
/// (idempotent: the same bytes give the same file).
pub fn save_pasted_image(img: &ClipImage, dir: &std::path::Path) -> Result<PathBuf, OxaError> {
    use std::os::unix::fs::OpenOptionsExt;
    let ext = match img.mime.as_str() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        other => {
            return Err(OxaError::new(
                codes::INVALID,
                format!("unsupported pasted image type {other}"),
            ))
        }
    };
    if img.data.is_empty() {
        return Err(OxaError::new(codes::INVALID, "the pasted image is empty"));
    }
    let digest = sha256(&img.data);
    let name: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(dir).map_err(|e| OxaError::io("create the paste directory", &e))?;
    let path = dir.join(format!("paste-{name}.{ext}"));
    if path.exists() {
        return Ok(path);
    }
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(&img.data));
    match written {
        Ok(()) => Ok(path),
        // Another writer produced the same content in the meantime.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(path),
        Err(e) => Err(OxaError::io("save the pasted image", &e)),
    }
}

/// A pasted image as a session part: the image is saved under `dir`, the part carries the
/// full SHA-256 of the bytes and the pixel size when the header tells it (PNG, GIF; else 0).
pub fn image_part(img: &ClipImage, dir: &Path) -> Result<Part, OxaError> {
    let path = save_pasted_image(img, dir)?;
    let (w, h) = image_dims(&img.data).unwrap_or((0, 0));
    Ok(Part::Image {
        path,
        mime: img.mime.clone(),
        sha256: sha256(&img.data),
        w,
        h,
    })
}

/// File paths in pasted or dropped text: the whole text must consist of one or more existing
/// absolute paths (shell-quoted, backslash-escaped or `file://` URLs, separated by spaces or
/// newlines); otherwise the result is empty and the text is pasted as text. `exists` answers
/// whether a path exists (injected for tests).
pub fn pasted_paths(text: &str, exists: &dyn Fn(&std::path::Path) -> bool) -> Vec<PathBuf> {
    let Some(tokens) = split_paste_tokens(text) else {
        return Vec::new();
    };
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(tokens.len());
    for token in tokens {
        let path = if token.url {
            parse_file_url(&token.text)
        } else {
            Some(token.text)
        };
        let Some(path) = path else {
            return Vec::new();
        };
        if !path.starts_with('/') {
            return Vec::new();
        }
        let path = PathBuf::from(path);
        if !exists(&path) {
            return Vec::new();
        }
        out.push(path);
    }
    out
}

/// `true` when the path's extension is an image type oxa attaches (`png`, `jpg`, `jpeg`,
/// `gif`, `webp`).
pub fn is_image_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        })
        .unwrap_or(false)
}

// ---- paste tokenizer -------------------------------------------------------------------

struct PasteToken {
    text: String,
    /// The token was a raw `file://` URL (percent-encoded, not shell-escaped).
    url: bool,
}

/// Split pasted text into shell-words. `None` when the text is not a plain list of words
/// (unterminated quote, dangling backslash). Whitespace outside quotes separates words.
fn split_paste_tokens(text: &str) -> Option<Vec<PasteToken>> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        // A raw file URL runs to the next whitespace, with no escape processing.
        let rest: String = chars[i..].iter().take(7).collect();
        if rest == "file://" {
            let mut word = String::new();
            while i < chars.len() && !chars[i].is_whitespace() {
                word.push(chars[i]);
                i += 1;
            }
            tokens.push(PasteToken {
                text: word,
                url: true,
            });
            continue;
        }
        let mut word = String::new();
        let mut quote: Option<char> = None;
        while i < chars.len() {
            let c = chars[i];
            match quote {
                Some('\'') => {
                    if c == '\'' {
                        quote = None;
                    } else {
                        word.push(c);
                    }
                }
                Some(_) => {
                    // Double quotes: backslash escapes only `"`, `\`, `$` and backtick.
                    if c == '"' {
                        quote = None;
                    } else if c == '\\'
                        && i + 1 < chars.len()
                        && matches!(chars[i + 1], '"' | '\\' | '$' | '`')
                    {
                        word.push(chars[i + 1]);
                        i += 1;
                    } else {
                        word.push(c);
                    }
                }
                None => {
                    if c.is_whitespace() {
                        break;
                    } else if c == '\'' || c == '"' {
                        quote = Some(c);
                    } else if c == '\\' {
                        i += 1;
                        if i >= chars.len() {
                            return None;
                        }
                        word.push(chars[i]);
                    } else {
                        word.push(c);
                    }
                }
            }
            i += 1;
        }
        if quote.is_some() {
            return None;
        }
        tokens.push(PasteToken {
            text: word,
            url: false,
        });
    }
    Some(tokens)
}

/// `file://[localhost]/path` with percent-encoding, to a path. `None` when malformed.
fn parse_file_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let s = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(s, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

// ---- image headers ---------------------------------------------------------------------

/// Pixel size from the PNG IHDR or the GIF logical screen descriptor.
fn image_dims(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() >= 24
        && data.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
        && &data[12..16] == b"IHDR"
    {
        let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return Some((w, h));
    }
    if data.len() >= 10 && (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
        let w = u16::from_le_bytes([data[6], data[7]]) as u32;
        let h = u16::from_le_bytes([data[8], data[9]]) as u32;
        return Some((w, h));
    }
    None
}

// ---- SHA-256 (FIPS 180-4) ----------------------------------------------------------------

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 of `data`. Used for the paste file names and `Part::Image::sha256`.
fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(v);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 1000 bytes: crosses several blocks and the length padding path.
        let long = vec![b'a'; 1000];
        assert_eq!(hex(&sha256(&long)).len(), 64);
    }

    #[test]
    fn png_and_gif_dimensions() {
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&640u32.to_be_bytes());
        png.extend_from_slice(&480u32.to_be_bytes());
        assert_eq!(image_dims(&png), Some((640, 480)));
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320u16.to_le_bytes());
        gif.extend_from_slice(&200u16.to_le_bytes());
        assert_eq!(image_dims(&gif), Some((320, 200)));
        assert_eq!(image_dims(b"not an image at all"), None);
    }

    #[test]
    fn extension_check_is_case_insensitive() {
        assert!(is_image_path(Path::new("/a/b.PNG")));
        assert!(is_image_path(Path::new("c.jpeg")));
        assert!(!is_image_path(Path::new("notes.md")));
    }

    #[test]
    fn tokenizer_rejects_unterminated_quotes() {
        assert!(split_paste_tokens("'/a b").is_none());
        assert!(split_paste_tokens("/a\\").is_none());
    }

    #[test]
    fn file_url_percent_decoding() {
        assert_eq!(
            parse_file_url("file:///tmp/a%20b.png").as_deref(),
            Some("/tmp/a b.png")
        );
        assert_eq!(
            parse_file_url("file://localhost/tmp/x").as_deref(),
            Some("/tmp/x")
        );
        assert_eq!(parse_file_url("file://relative/x"), None);
    }
}
