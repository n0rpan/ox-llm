//! A6: the `read` tool.
// ported from pi packages/coding-agent/src/core/tools/read.ts
use crate::descriptions::{self, diet, errors, limits};
use base64::Engine as _;
use oxa_proto::{image_placeholder, Access, Tool, ToolCtx, ToolOutput, ToolSpec, ToolTune, Value, WireImage};
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

/// Bytes read to tell a binary file from text: a NUL byte in this prefix means binary.
const BINARY_PROBE: u64 = 8 * 1024;
/// Bytes read to find a JPEG frame header. Markers before the frame are short in practice.
const JPEG_HEADER_MAX: u64 = 1024 * 1024;
/// L4: bytes kept free in a read for the notice after the content (at most 256, an eighth of
/// the cap for a small one).
const NOTICE_ROOM: usize = 256;
/// Chunk size of the streaming line scan. Only one chunk is in memory at a time.
const SCAN_CHUNK: usize = 64 * 1024;
/// Model-facing text of an argument error. Not in `descriptions`: pi has no fixed text for it.
const INVALID_PATH_ARG: &str = "Invalid arguments: path must be a string";

/// `read {path, offset?, limit?}`: text with pi's paging (`offset` 1-based, `limit` lines; 2000 lines / 50 KB per call with pi's continuation notice). Large files are streamed, never loaded whole. A binary file is refused with pi's text. An image (PNG, JPEG, GIF, WebP) returns `oxa_proto::image_placeholder` (name, width, height read from the header) and nothing else, unless the model takes images (`ToolTune::image_input`): then the image is returned as an image part, within `ToolTune::max_image_bytes`.
#[derive(Debug, Clone)]
pub struct ReadTool {
    spec: ToolSpec,
}

impl ReadTool {
    pub fn new() -> ReadTool {
        ReadTool {
            spec: crate::spec_of(&descriptions::builtin_specs::READ_DEFAULT),
        }
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for ReadTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// `[Access::Read(path)]`.
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        let path = arg_path(args)?;
        Ok(vec![Access::Read(crate::resolve_tool_path(cx, path))])
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        read_file(args, cx).unwrap_or_else(ToolOutput::error)
    }
}

/// The `path` argument, exactly as the model wrote it. Shared with `write`.
pub(crate) fn arg_path(args: &Value) -> Result<&str, String> {
    args.get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| INVALID_PATH_ARG.to_string())
}

/// An optional numeric argument. Shared with nothing else; `offset` and `limit` only.
fn arg_number(args: &Value, key: &str) -> Result<Option<f64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) if n.is_finite() => Ok(Some(n)),
            _ => Err(format!("Invalid arguments: {key} must be a number")),
        },
    }
}

/// The whole read. `Ok` is the output for the model (pi's notices included, an image part for an
/// image the model takes); `Err` is a tool error.
fn read_file(args: &Value, cx: &ToolCtx) -> Result<ToolOutput, String> {
    let path = arg_path(args)?;
    let offset = arg_number(args, "offset")?;
    let limit = arg_number(args, "limit")?;
    let abs = crate::resolve_tool_path(cx, path);
    let meta = match std::fs::metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(errors::path_not_found(path)),
        Err(e) => return Err(read_io_error(path, &e)),
    };
    if !meta.is_file() {
        return Err(format!("Not a regular file: {path}"));
    }
    if cx.cancel.is_cancelled() {
        return Err(errors::ABORTED.to_string());
    }

    // Images first: their headers contain NUL bytes, so the binary test must not see them.
    let probe = read_probe(&abs, path, cx)?;
    if let Some((w, h)) = image_size(&abs, &probe, cx).map_err(|e| read_io_error(path, &e))? {
        return read_image(&abs, path, &probe, (w, h), meta.len(), cx);
    }
    if probe.contains(&0) {
        return Err(errors::read_binary_refused(path));
    }

    // Pi's 1-based offset and line limit. A negative limit selects nothing (pi's slice would
    // select odd lines; oxa does not reproduce that).
    let start = offset.map_or(0, |o| (o.max(0.0).floor() as usize).saturating_sub(1));
    let end = match limit {
        Some(l) => start.saturating_add(l.max(0.0).floor() as usize),
        None => usize::MAX,
    };
    // TOOL-DIET (T8, R5): a call with neither `offset` nor `limit` gets the first page of a
    // larger file, if the tuning sets one. Any other call is pi's read, cut at the tool output
    // cap of the tuning (pi's 16 KiB when it sets none).
    let mut first_page = offset.is_none() && limit.is_none() && cx.tune.read_page_lines > 0;
    // L4: the first page is cut at the page cap, any other call at the tool output cap. The
    // notice is written inside that cap too (the output cap of `shape` then never cuts a
    // notice away).
    let (mut max_lines, mut cap, mut content) = if first_page {
        let cap = page_cap(&cx.tune);
        let content = if cx.tune.read_page_bytes > 0 {
            body_room(cap)
        } else {
            cap
        };
        (cx.tune.read_page_lines as usize, cap, content)
    } else {
        let cap = output_cap(&cx.tune);
        (limits::READ_MAX_LINES, cap, body_cap(&cx.tune))
    };
    let mut sc = scan_lines(&abs, path, cx, start, end, content, max_lines)?;
    let total = sc.total;
    if start >= total {
        return Err(errors::read_offset_beyond_end(start + 1, total));
    }

    let mut shown = shape_selection(&sc, content);
    if shown.first_exceeds && first_page {
        // The page is smaller than pi's cap: a first line that only the page refuses is read
        // as pi reads it, so the page never hides what a plain read shows.
        first_page = false;
        (max_lines, cap, content) = (
            limits::READ_MAX_LINES,
            limits::TOOL_OUTPUT_CAP_DEFAULT,
            limits::TOOL_OUTPUT_CAP_DEFAULT,
        );
        sc = scan_lines(&abs, path, cx, start, end, content, max_lines)?;
        shown = shape_selection(&sc, content);
    }
    if shown.first_exceeds {
        // The limit named is the room the line had (pi: the cap itself).
        return Ok(ToolOutput::ok(errors::read_line_too_long(
            start + 1,
            sc.kept[0].len,
            content,
            path,
        )));
    }
    let mut text = shown.content;
    let last = start + shown.lines;
    // The page hint goes inside the first page's notice, before its closing bracket.
    let hint = if first_page { diet::READ_PAGE_HINT } else { "" };
    match shown.cut {
        Some(Cut::Lines) => {
            text.push_str("\n\n");
            let notice = errors::read_more_by_lines(start + 1, last, total, last + 1);
            text.push_str(&with_hint(&notice, hint));
        }
        Some(Cut::Bytes) => {
            text.push_str("\n\n");
            let notice = errors::read_more_by_bytes(start + 1, last, total, cap, last + 1);
            text.push_str(&with_hint(&notice, hint));
        }
        None => {
            // The caller's limit stopped early while the file has more lines.
            let shown_end = start + sc.selected;
            if shown_end < total {
                text.push_str("\n\n");
                text.push_str(&errors::read_more_after_limit(total - shown_end, shown_end + 1));
            }
        }
    }
    Ok(ToolOutput::ok(text))
}

/// B2b: the result of `read` on an image. The placeholder line, unless the model takes images
/// (`tune.image_input`) and the file is a type it takes (PNG, JPEG, GIF, WebP): then the image is
/// returned as an image part. An image over `tune.max_image_bytes` (0 = no limit) is not sent;
/// the line names the size, the limit and the setting. The bytes come through the same pinned
/// open as any read, so a private path or a swapped link is refused before any byte is sent.
fn read_image(
    abs: &Path,
    shown: &str,
    head: &[u8],
    (w, h): (u32, u32),
    size: u64,
    cx: &ToolCtx,
) -> Result<ToolOutput, String> {
    let placeholder = image_placeholder(shown, w, h);
    let Some(mime) = image_mime(head).filter(|_| cx.tune.image_input) else {
        return Ok(image_read_out(placeholder));
    };
    let max = u64::from(cx.tune.max_image_bytes);
    if max > 0 && size > max {
        return Ok(image_read_out(format!(
            "[image {shown} {w}x{h}: not sent, {size} bytes is over the limit of {max} (oxa.read.maxImageBytes)]"
        )));
    }
    let mut file = crate::checked::open(cx, abs).map_err(|e| read_io_error(shown, &e))?;
    let bytes = read_image_bytes(&mut file, max).map_err(|e| read_io_error(shown, &e))?;
    let bytes = match bytes {
        ImageBytes::Within(bytes) => bytes,
        ImageBytes::OverLimit(actual) => {
            return Ok(image_read_out(format!(
                "[image {shown} {w}x{h}: not sent, at least {actual} bytes is over the limit of {max} (oxa.read.maxImageBytes)]"
            )))
        }
    };
    let mut out = image_read_out(format!("[image {shown} {w}x{h}: shown to the model]"));
    out.images = vec![WireImage {
        mime: mime.to_string(),
        base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
    }];
    Ok(out)
}

/// An image-read result, marked for the finish decision (`oxa.done.imageCrossCheck`): the run
/// consulted an image file, even when the model only got the placeholder line. The marker is
/// display-only state (`ToolOutput::ui`); it never reaches the wire.
fn image_read_out(text: String) -> ToolOutput {
    let mut out = ToolOutput::ok(text);
    out.ui = serde_json::json!({ "imageRead": true });
    out
}

enum ImageBytes {
    Within(Vec<u8>),
    OverLimit(u64),
}

/// Read at most one byte beyond the configured cap, so a file that grows after its metadata was
/// inspected cannot turn the image path into an unbounded allocation.
fn read_image_bytes(reader: &mut impl Read, max: u64) -> io::Result<ImageBytes> {
    let mut bytes = Vec::new();
    if max == 0 {
        reader.read_to_end(&mut bytes)?;
        return Ok(ImageBytes::Within(bytes));
    }
    reader.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        Ok(ImageBytes::OverLimit(bytes.len() as u64))
    } else {
        Ok(ImageBytes::Within(bytes))
    }
}

/// The media type of an image the model takes (PNG, JPEG, GIF or WebP), from its first bytes.
/// `None` for any other file (a BMP is a placeholder).
fn image_mime(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(&b"WEBP"[..]) {
        Some("image/webp")
    } else {
        None
    }
}

/// L4: the byte cap of the first page of `read`: the tuned page cap (`read_page_bytes`, at
/// most 8 KiB and within the output cap), or pi's 16 KiB when the tuning sets none.
fn page_cap(tune: &ToolTune) -> usize {
    match tune.read_page_bytes {
        0 => limits::TOOL_OUTPUT_CAP_DEFAULT,
        b => b as usize,
    }
}

/// L4: the byte cap of `grep`, `find`, `ls` and a `read` with `offset` / `limit`: the tool
/// output cap of the tuning (`output_cap_bytes`, which an owner setting or a cloud window may
/// raise above pi's 16 KiB), or pi's 16 KiB when the tuning sets none. The output cap of `shape` stays the last word on every result.
pub(crate) fn output_cap(tune: &ToolTune) -> usize {
    match tune.output_cap_bytes {
        0 => limits::TOOL_OUTPUT_CAP_DEFAULT,
        b => b as usize,
    }
}

/// L4: whether the tuning sets a tool output cap: the cut is then written inside it and names
/// the way on.
pub(crate) fn budgeted(tune: &ToolTune) -> bool {
    tune.output_cap_bytes > 0
}

/// L4: the bytes of the content of a result cut at [`output_cap`] (its notice needs the rest):
/// the cap less [`body_room`]'s room when [`budgeted`], else the cap itself (pi).
pub(crate) fn body_cap(tune: &ToolTune) -> usize {
    let cap = output_cap(tune);
    if budgeted(tune) {
        body_room(cap)
    } else {
        cap
    }
}

/// `cap` less a room for the notice of at most [`NOTICE_ROOM`] (an eighth of a small cap).
fn body_room(cap: usize) -> usize {
    cap - (cap / 8).min(NOTICE_ROOM)
}

/// `notice` with `hint` put before its closing `]` (a notice that has none is returned as is).
fn with_hint(notice: &str, hint: &str) -> String {
    match notice.strip_suffix(']') {
        Some(body) if !hint.is_empty() => format!("{body}{hint}]"),
        _ => notice.to_string(),
    }
}

/// The first bytes of a file, enough for the image and binary tests.
fn read_probe(abs: &Path, shown: &str, cx: &ToolCtx) -> Result<Vec<u8>, String> {
    let file = crate::checked::open(cx, abs).map_err(|e| read_io_error(shown, &e))?;
    let mut probe = Vec::new();
    file.take(BINARY_PROBE)
        .read_to_end(&mut probe)
        .map_err(|e| read_io_error(shown, &e))?;
    Ok(probe)
}

/// The one I/O failure text of `read` (pi reports the node error; oxa reports the OS error).
fn read_io_error(shown: &str, e: &io::Error) -> String {
    format!("Could not read file: {shown}. {e}")
}

/// A line the output may show: its first `cap + 1` bytes and its full length in bytes.
struct Seg {
    head: Vec<u8>,
    len: usize,
}

/// What one streaming pass over a file learned. `kept` holds the selected lines that truncation
/// can show (the first one that does not fit is the last kept); the counts are exact.
struct Scan {
    start: usize,
    end: usize,
    cap: usize,
    /// Most lines one output may show (2000 for pi; the first-page size for a first page).
    max_lines: usize,
    /// Lines of the whole file, as pi counts them: newline count + 1.
    total: usize,
    /// Selected lines: `start..min(end, total)`.
    selected: usize,
    /// Byte length of the selected lines joined with `\n` (pi's `totalBytes`).
    selected_bytes: usize,
    /// Whether the last selected line is empty (the file ended in `\n`).
    last_empty: bool,
    kept: Vec<Seg>,
    kept_bytes: usize,
    /// Set once `kept` cannot grow: more lines would never be shown.
    full: bool,
}

impl Scan {
    fn new(start: usize, end: usize, cap: usize, max_lines: usize) -> Scan {
        Scan {
            start,
            end,
            cap,
            max_lines,
            total: 0,
            selected: 0,
            selected_bytes: 0,
            last_empty: false,
            kept: Vec::new(),
            kept_bytes: 0,
            full: false,
        }
    }

    fn in_range(&self, line: usize) -> bool {
        line >= self.start && line < self.end
    }

    /// Record one selected line, in order.
    fn take(&mut self, head: Vec<u8>, len: usize) {
        let sep = usize::from(self.selected > 0);
        self.selected += 1;
        self.selected_bytes += len + sep;
        self.last_empty = len == 0;
        if !self.full {
            self.kept_bytes += len + sep;
            self.kept.push(Seg { head, len });
            if self.kept.len() > self.max_lines || self.kept_bytes > self.cap {
                self.full = true;
            }
        }
    }
}

/// Append a piece of the current line. Only the first `cap + 1` bytes are stored: a longer line is
/// never shown in full, and the length is all the notices need.
fn keep_piece(head: &mut Vec<u8>, len: &mut usize, piece: &[u8], cap: usize) {
    *len += piece.len();
    let room = (cap + 1).saturating_sub(head.len());
    head.extend_from_slice(&piece[..piece.len().min(room)]);
}

/// One pass over the file, in chunks: counts every line, keeps the selected ones as far as they
/// can be shown. Memory does not grow with the file.
fn scan_lines(
    abs: &Path,
    shown: &str,
    cx: &ToolCtx,
    start: usize,
    end: usize,
    cap: usize,
    max_lines: usize,
) -> Result<Scan, String> {
    let file = crate::checked::open(cx, abs).map_err(|e| read_io_error(shown, &e))?;
    let mut reader = BufReader::with_capacity(SCAN_CHUNK, file);
    let mut sc = Scan::new(start, end, cap, max_lines);
    let mut line = 0usize;
    let mut sel = sc.in_range(0);
    let mut head: Vec<u8> = Vec::new();
    let mut len = 0usize;
    loop {
        let buf = reader.fill_buf().map_err(|e| read_io_error(shown, &e))?;
        if buf.is_empty() {
            break;
        }
        if cx.cancel.is_cancelled() {
            return Err(errors::ABORTED.to_string());
        }
        let n = buf.len();
        let mut pos = 0;
        while pos < n {
            match buf[pos..].iter().position(|&b| b == b'\n') {
                Some(k) => {
                    if sel {
                        keep_piece(&mut head, &mut len, &buf[pos..pos + k], cap);
                        sc.take(std::mem::take(&mut head), len);
                    }
                    len = 0;
                    pos += k + 1;
                    line += 1;
                    sel = sc.in_range(line);
                }
                None => {
                    if sel {
                        keep_piece(&mut head, &mut len, &buf[pos..n], cap);
                    }
                    pos = n;
                }
            }
        }
        reader.consume(n);
    }
    // The last line has no newline after it (it may be empty).
    if sel {
        sc.take(head, len);
    }
    sc.total = line + 1;
    Ok(sc)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cut {
    Lines,
    Bytes,
}

/// pi's `truncateHead` over the selected lines.
struct Shown {
    content: String,
    /// Lines in `content`.
    lines: usize,
    /// Which limit cut the output, if one did.
    cut: Option<Cut>,
    /// The first line alone exceeds the byte cap: `content` is empty and the notice replaces it.
    first_exceeds: bool,
}

fn shape_selection(sc: &Scan, cap: usize) -> Shown {
    let max_lines = sc.max_lines;
    // pi's counting drops the trailing empty line of a text that ends in `\n`.
    let counting = if sc.selected == 0 {
        0
    } else if sc.last_empty {
        sc.selected - 1
    } else {
        sc.selected
    };
    if counting <= max_lines && sc.selected_bytes <= cap {
        let content = sc
            .kept
            .iter()
            .map(|s| String::from_utf8_lossy(&s.head))
            .collect::<Vec<_>>()
            .join("\n");
        return Shown {
            content,
            lines: counting,
            cut: None,
            first_exceeds: false,
        };
    }
    let first_len = sc.kept.first().map_or(0, |s| s.len);
    if first_len > cap {
        return Shown {
            content: String::new(),
            lines: 0,
            cut: Some(Cut::Bytes),
            first_exceeds: true,
        };
    }
    let mut used = 0usize;
    let mut n = 0usize;
    let mut cut = Cut::Lines;
    for i in 0..counting.min(max_lines) {
        let Some(seg) = sc.kept.get(i) else { break };
        let size = seg.len + usize::from(i > 0);
        if used + size > cap {
            cut = Cut::Bytes;
            break;
        }
        used += size;
        n = i + 1;
    }
    let content = sc.kept[..n]
        .iter()
        .map(|s| String::from_utf8_lossy(&s.head))
        .collect::<Vec<_>>()
        .join("\n");
    Shown {
        content,
        lines: n,
        cut: Some(cut),
        first_exceeds: false,
    }
}

/// Little-endian unsigned integer of `n` bytes at `i`.
fn le(b: &[u8], i: usize, n: usize) -> Option<u32> {
    let s = b.get(i..i.checked_add(n)?)?;
    Some(s.iter().rev().fold(0u32, |acc, &x| (acc << 8) | u32::from(x)))
}

/// Big-endian unsigned integer of `n` bytes at `i`.
fn be(b: &[u8], i: usize, n: usize) -> Option<u32> {
    let s = b.get(i..i.checked_add(n)?)?;
    Some(s.iter().fold(0u32, |acc, &x| (acc << 8) | u32::from(x)))
}

/// Width and height of a PNG, GIF, BMP or WebP image from its header; JPEG is read further.
/// `None` when the file is not one of these, or its header is cut short.
fn image_size(abs: &Path, head: &[u8], cx: &ToolCtx) -> io::Result<Option<(u32, u32)>> {
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        let mut buf = Vec::new();
        crate::checked::open(cx, abs)?
            .take(JPEG_HEADER_MAX)
            .read_to_end(&mut buf)?;
        return Ok(jpeg_frame(&buf));
    }
    Ok(header_size(head))
}

fn header_size(head: &[u8]) -> Option<(u32, u32)> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        if head.get(12..16)? != b"IHDR" {
            return None;
        }
        return Some((be(head, 16, 4)?, be(head, 20, 4)?));
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some((le(head, 6, 2)?, le(head, 8, 2)?));
    }
    if head.starts_with(b"BM") {
        // The DIB header size must be a known BMP one, or any text starting with "BM" is an image.
        if !matches!(le(head, 14, 4)?, 12 | 40 | 52 | 56 | 64 | 108 | 124) {
            return None;
        }
        // BITMAPINFOHEADER: signed width and height; a negative height is a top-down bitmap.
        let w = le(head, 18, 4)? as i32;
        let h = le(head, 22, 4)? as i32;
        return Some((w.unsigned_abs(), h.unsigned_abs()));
    }
    if head.starts_with(b"RIFF") && head.get(8..12)? == b"WEBP" {
        let kind = head.get(12..16)?;
        if kind == b"VP8 " {
            if head.get(23..26)? != &[0x9d, 0x01, 0x2a][..] {
                return None;
            }
            return Some((le(head, 26, 2)? & 0x3fff, le(head, 28, 2)? & 0x3fff));
        }
        if kind == b"VP8L" {
            if *head.get(20)? != 0x2f {
                return None;
            }
            let bits = le(head, 21, 4)?;
            return Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1));
        }
        if kind == b"VP8X" {
            return Some((le(head, 24, 3)? + 1, le(head, 27, 3)? + 1));
        }
    }
    None
}

/// The first start-of-frame segment of a JPEG: its height and width.
fn jpeg_frame(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2;
    while i + 1 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        i += 2;
        if marker == 0xFF {
            // A fill byte: the marker starts one byte later.
            i -= 1;
            continue;
        }
        if matches!(marker, 0x01 | 0xD0..=0xD8) {
            // Standalone markers carry no length.
            continue;
        }
        if marker == 0xD9 {
            return None;
        }
        let seg = be(b, i, 2)? as usize;
        if seg < 2 {
            return None;
        }
        if (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC) {
            // Segment: length (2), precision (1), height (2), width (2).
            return Some((be(b, i + 5, 2)?, be(b, i + 3, 2)?));
        }
        i += seg;
    }
    None
}

#[cfg(test)]
mod image_read_tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn actual_image_read_is_bounded_to_limit_plus_one() {
        let mut input = Cursor::new(vec![7; 32]);
        assert!(matches!(
            read_image_bytes(&mut input, 8).unwrap(),
            ImageBytes::OverLimit(9)
        ));
        assert_eq!(input.position(), 9);
    }

    #[test]
    fn zero_image_limit_reads_the_whole_input() {
        let mut input = Cursor::new(vec![7; 32]);
        assert!(matches!(
            read_image_bytes(&mut input, 0).unwrap(),
            ImageBytes::Within(bytes) if bytes.len() == 32
        ));
    }
}
