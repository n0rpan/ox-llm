//! C14 acceptance tests: Images and OSC sequences. Owner: task C14.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use base64::Engine;
use oxa_tui::{
    detect_caps, encode_iterm2, encode_kitty, hyperlink, image_cell_size, image_dims, image_fallback,
    is_image_line, notify_sequence, osc52_copy, render_image, sniff_image_mime, title_sequence, ColorMode,
    ImageDims, ImageProtocol, COLOR_QUERY,
};

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A PNG header of the given size (enough for dimension reading).
fn png_header(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(&13u32.to_be_bytes());
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0]);
    v
}

/// The payloads of every kitty chunk in `seq`, concatenated.
fn kitty_payload(seq: &str) -> (String, usize) {
    let mut payload = String::new();
    let mut chunks = 0;
    for part in seq.split("\x1b\\").filter(|p| !p.is_empty()) {
        chunks += 1;
        let body = part.strip_prefix("\x1b_G").expect("chunk starts with APC G");
        let (_, data) = body.split_once(';').expect("chunk has a ';' separator");
        payload.push_str(data);
    }
    (payload, chunks)
}

/// chunked base64
#[test]
fn kitty_encoder_golden() {
    // Single chunk: fixed parameters, cols/rows/id, payload after ';'.
    assert_eq!(
        encode_kitty(b"hello", 3, 2, 7),
        "\x1b_Ga=T,f=100,q=2,c=3,r=2,i=7;aGVsbG8=\x1b\\"
    );
    // id 0 is reserved by kitty and omitted.
    assert_eq!(
        encode_kitty(b"hello", 3, 2, 0),
        "\x1b_Ga=T,f=100,q=2,c=3,r=2;aGVsbG8=\x1b\\"
    );

    // 10 000 bytes -> 13 336 base64 chars -> chunks of 4096, 4096, 4096, 1048.
    let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    let seq = encode_kitty(&data, 10, 4, 9);
    let parts: Vec<&str> = seq.split("\x1b\\").filter(|p| !p.is_empty()).collect();
    assert_eq!(parts.len(), 4);
    assert!(parts[0].starts_with("\x1b_Ga=T,f=100,q=2,c=10,r=4,i=9,m=1;"));
    assert!(parts[1].starts_with("\x1b_Gm=1;"));
    assert!(parts[2].starts_with("\x1b_Gm=1;"));
    assert!(parts[3].starts_with("\x1b_Gm=0;"));
    let (payload, chunks) = kitty_payload(&seq);
    assert_eq!(chunks, 4);
    assert_eq!(payload.len(), b64(&data).len());
    let decoded = base64::engine::general_purpose::STANDARD.decode(payload).unwrap();
    assert_eq!(decoded, data);
}

#[test]
fn kitty_delete_golden() {
    assert_eq!(oxa_tui::delete_kitty_image(42), "\x1b_Ga=d,d=I,i=42,q=2\x1b\\");
}

/// OSC 1337
#[test]
fn iterm2_encoder_golden() {
    assert_eq!(
        encode_iterm2(b"hello", 3, 2, None),
        "\x1b]1337;File=inline=1;width=3;height=2;preserveAspectRatio=1:aGVsbG8=\x07"
    );
    let named = encode_iterm2(b"hello", 3, 2, Some("a.png"));
    assert_eq!(
        named,
        format!(
            "\x1b]1337;File=inline=1;width=3;height=2;preserveAspectRatio=1;name={}:aGVsbG8=\x07",
            b64(b"a.png")
        )
    );
    assert_eq!(b64(b"a.png"), "YS5wbmc=");
}

/// env-driven table (passed in, not read)
#[test]
fn detection_overrides() {
    // Kitty, ghostty, wezterm -> kitty images.
    let caps = detect_caps(&env(&[("TERM_PROGRAM", "ghostty")]));
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
    assert!(caps.hyperlinks);
    let caps = detect_caps(&env(&[("KITTY_WINDOW_ID", "1")]));
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
    let caps = detect_caps(&env(&[("WEZTERM_PANE", "0")]));
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));

    // iTerm2 -> iterm2 images.
    let caps = detect_caps(&env(&[("TERM_PROGRAM", "iTerm.app")]));
    assert_eq!(caps.images, Some(ImageProtocol::Iterm2));

    // tmux and screen -> no images and no OSC 8, even inside a kitty-capable terminal.
    let caps = detect_caps(&env(&[
        ("TMUX", "/tmp/tmux-1000/default,1,0"),
        ("KITTY_WINDOW_ID", "1"),
    ]));
    assert_eq!(caps.images, None);
    assert!(!caps.hyperlinks);
    let caps = detect_caps(&env(&[("TERM", "screen-256color"), ("KITTY_WINDOW_ID", "1")]));
    assert_eq!(caps.images, None);
    assert!(!caps.hyperlinks);

    // Truecolor from COLORTERM; 256 colours otherwise.
    assert_eq!(
        detect_caps(&env(&[("COLORTERM", "truecolor")])).color,
        ColorMode::TrueColor
    );
    assert_eq!(
        detect_caps(&env(&[("COLORTERM", "24bit")])).color,
        ColorMode::TrueColor
    );
    assert_eq!(
        detect_caps(&env(&[("TERM", "xterm-256color")])).color,
        ColorMode::Ansi256
    );

    // Unknown terminal: conservative defaults.
    let caps = detect_caps(&env(&[("TERM", "xterm")]));
    assert_eq!(caps.images, None);
    assert!(!caps.hyperlinks);
    assert!(!caps.osc52);
    assert!(!caps.kitty_keyboard);
    assert_eq!(caps.cell_px, None);
}

/// OSC 8: open/close
#[test]
fn osc8_hyperlink() {
    assert_eq!(
        hyperlink("docs", "https://example.com/a"),
        "\x1b]8;;https://example.com/a\x1b\\docs\x1b]8;;\x1b\\"
    );
    // Control characters in the URL are removed; the text is kept as given.
    assert_eq!(
        hyperlink("x", "https://a\x07\x1b\nb"),
        "\x1b]8;;https://ab\x1b\\x\x1b]8;;\x1b\\"
    );
}

/// OSC 52: base64 payload
#[test]
fn osc52_clipboard() {
    assert_eq!(osc52_copy("hi", false), "\x1b]52;c;aGk=\x1b\\");
    assert_eq!(osc52_copy("", false), "\x1b]52;c;\x1b\\");
    // Inside tmux the sequence is wrapped in DCS passthrough with every ESC doubled.
    assert_eq!(
        osc52_copy("hi", true),
        "\x1bPtmux;\x1b\x1b]52;c;aGk=\x1b\x1b\\\x1b\\"
    );
}

#[test]
fn title_and_notify_sequences() {
    assert_eq!(title_sequence("a\x1bb"), "\x1b]0;ab\x07");
    assert_eq!(notify_sequence("hi\n"), "\x1b]9;hi\x07");
    assert_eq!(COLOR_QUERY, "\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c");
}

/// png/jpeg/gif/webp
#[test]
fn image_dimensions_from_header() {
    // PNG: IHDR width and height, big-endian.
    assert_eq!(
        image_dims(&png_header(640, 480), "image/png"),
        Some(ImageDims { w: 640, h: 480 })
    );
    assert_eq!(
        image_dims(&png_header(640, 480)[..20], "image/png"),
        None,
        "truncated PNG"
    );

    // GIF: logical screen width and height, little-endian.
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&300u16.to_le_bytes());
    gif.extend_from_slice(&200u16.to_le_bytes());
    assert_eq!(image_dims(&gif, "image/gif"), Some(ImageDims { w: 300, h: 200 }));

    // WebP VP8X: canvas width-1 and height-1 as 24-bit values.
    let mut webp = b"RIFF\x00\x00\x00\x00WEBPVP8X".to_vec();
    webp.extend_from_slice(&10u32.to_le_bytes());
    webp.extend_from_slice(&[0, 0, 0, 0]);
    webp.extend_from_slice(&[(799 & 0xff) as u8, (799 >> 8) as u8, 0]);
    webp.extend_from_slice(&[(599 & 0xff) as u8, (599 >> 8) as u8, 0]);
    assert_eq!(
        image_dims(&webp, "image/webp"),
        Some(ImageDims { w: 800, h: 600 })
    );

    // WebP VP8L: 14-bit width-1 and height-1 packed after the 0x2f signature.
    let bits: u32 = (119) | ((89) << 14);
    let mut vp8l = b"RIFF\x00\x00\x00\x00WEBPVP8L".to_vec();
    vp8l.extend_from_slice(&5u32.to_le_bytes());
    vp8l.push(0x2f);
    vp8l.extend_from_slice(&bits.to_le_bytes());
    assert_eq!(image_dims(&vp8l, "image/webp"), Some(ImageDims { w: 120, h: 90 }));

    // WebP VP8 (lossy): 14-bit width and height after the start code.
    let mut vp8 = b"RIFF\x00\x00\x00\x00WEBPVP8 ".to_vec();
    vp8.extend_from_slice(&10u32.to_le_bytes());
    vp8.extend_from_slice(&[0, 0, 0]);
    vp8.extend_from_slice(&[0x9d, 0x01, 0x2a]);
    vp8.extend_from_slice(&64u16.to_le_bytes());
    vp8.extend_from_slice(&32u16.to_le_bytes());
    assert_eq!(image_dims(&vp8, "image/webp"), Some(ImageDims { w: 64, h: 32 }));

    // JPEG: scan segments until the first SOF (skipping APP0), then height and width.
    let mut jpeg = vec![0xff, 0xd8];
    jpeg.extend_from_slice(&[0xff, 0xe0, 0x00, 0x04, 0x00, 0x00]);
    jpeg.extend_from_slice(&[
        0xff, 0xc0, 0x00, 0x0b, 0x08, 0x01, 0xf4, 0x02, 0x80, 0x01, 0x11, 0x00,
    ]);
    assert_eq!(
        image_dims(&jpeg, "image/jpeg"),
        Some(ImageDims { w: 640, h: 500 })
    );
    assert_eq!(image_dims(&jpeg[..10], "image/jpeg"), None, "truncated JPEG");

    // Unknown MIME types and wrong magic are not read.
    assert_eq!(image_dims(&png_header(1, 1), "image/bmp"), None);
    assert_eq!(image_dims(b"not an image at all, really", "image/gif"), None);
}

#[test]
fn sniff_and_cell_size() {
    assert_eq!(sniff_image_mime(&png_header(1, 1)), Some("image/png"));
    assert_eq!(sniff_image_mime(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
    assert_eq!(sniff_image_mime(b"GIF89a...."), Some("image/gif"));
    assert_eq!(sniff_image_mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
    assert_eq!(sniff_image_mime(b"%PDF-1.4"), None);

    // Default cell 9x18 px. 900x180 px natural is 100x10 cells: fits 120x20 unchanged.
    let wide = ImageDims { w: 900, h: 180 };
    assert_eq!(image_cell_size(wide, 120, 20, None), (100, 10));
    // Limited to 50 columns: scaled to 50 cols (keeps aspect: 5 rows).
    assert_eq!(image_cell_size(wide, 50, 20, None), (50, 5));
    // Never enlarged.
    assert_eq!(image_cell_size(ImageDims { w: 9, h: 18 }, 80, 40, None), (1, 1));
    // Explicit cell size is used.
    // 800x400 px at 8x16 px cells: scale 0.8 -> 640x320 px -> 80x20 cells.
    assert_eq!(
        image_cell_size(ImageDims { w: 800, h: 400 }, 80, 40, Some((8, 16))),
        (80, 20)
    );
    // Row limit wins when it is the tighter box: 900x180 px in 3 rows -> 30x3 cells.
    assert_eq!(image_cell_size(wide, 120, 3, None), (30, 3));
    // A zero box fits nothing.
    assert_eq!(image_cell_size(wide, 0, 20, None), (0, 0));
}

#[test]
fn render_image_layout_and_fallback() {
    let png = png_header(900, 180);
    let lines = render_image(&png, "image/png", ImageProtocol::Kitty, 50, 10, None, 5);
    // 50 columns -> 5 rows: the sequence on the first line, 4 empty lines reserving the space.
    assert_eq!(lines.len(), 5);
    assert!(is_image_line(&lines[0]));
    assert!(lines[0].contains("c=50,r=5,i=5"));
    // A 3-row limit clamps the same image to 30x3 cells.
    let clamped = render_image(&png, "image/png", ImageProtocol::Kitty, 50, 3, None, 5);
    assert_eq!(clamped.len(), 3);
    assert!(clamped[0].contains("c=30,r=3"));
    assert!(lines[1..].iter().all(|l| l.is_empty()));

    let lines = render_image(&png, "image/png", ImageProtocol::Iterm2, 50, 10, None, 5);
    assert_eq!(lines.len(), 5);
    assert!(is_image_line(&lines[0]));
    assert!(lines[0].starts_with("\x1b]1337;File=inline=1;width=50;height=5;"));

    // Kitty only accepts PNG payloads; other formats fall back to text.
    let gif = {
        let mut g = b"GIF89a".to_vec();
        g.extend_from_slice(&4u16.to_le_bytes());
        g.extend_from_slice(&4u16.to_le_bytes());
        g
    };
    let lines = render_image(&gif, "image/gif", ImageProtocol::Kitty, 50, 3, None, 1);
    assert_eq!(lines, vec!["[image 4x4]".to_string()]);
    // Unreadable dimensions fall back too.
    assert_eq!(
        render_image(b"junk", "image/png", ImageProtocol::Kitty, 50, 3, None, 1),
        vec!["[image]".to_string()]
    );
    // Zero rows reserve nothing.
    assert!(render_image(&png, "image/png", ImageProtocol::Kitty, 50, 0, None, 1).is_empty());

    assert_eq!(
        image_fallback("image/png", Some(ImageDims { w: 3, h: 2 }), Some("logo.png")),
        "[image logo.png 3x2]"
    );
    assert_eq!(
        image_fallback("image/png", None, Some("logo.png")),
        "[image logo.png]"
    );
    assert_eq!(
        image_fallback("image/png", Some(ImageDims { w: 3, h: 2 }), None),
        "[image 3x2]"
    );
    assert_eq!(image_fallback("image/png", None, None), "[image]");
    assert!(!is_image_line("plain text \x1b[31mred\x1b[0m"));
}

/// The start-up query replies (`COLOR_QUERY`): OSC 10 / 11 with either terminator (BEL or
/// ST), the colour payload in 16-bit and 8-bit forms.
#[test]
fn startup_query_replies_parse() {
    use oxa_tui::{parse_input, parse_osc_color, InputEvent};
    let reply = |code: u16, data: &str| InputEvent::OscReply {
        code,
        data: data.to_string(),
    };
    let buf: &[u8] = b"\x1b]10;rgb:d0d0/d0d0/d0d0\x07\x1b]11;rgb:1c/1c/1c\x1b\\";
    let (events, used) = parse_input(buf);
    assert_eq!(used, buf.len());
    assert_eq!(
        events,
        vec![reply(10, "rgb:d0d0/d0d0/d0d0"), reply(11, "rgb:1c/1c/1c"),]
    );
    assert_eq!(parse_osc_color("rgb:d0d0/d0d0/d0d0"), Some((0xd0, 0xd0, 0xd0)));
    assert_eq!(parse_osc_color("rgb:1c/1c/1c"), Some((0x1c, 0x1c, 0x1c)));
    assert_eq!(parse_osc_color("rgb:ffff/ffff/ffff"), Some((255, 255, 255)));
    // An unfinished reply is not consumed (it waits for its terminator).
    assert_eq!(parse_input(b"\x1b]11;rgb:ffff/ff").1, 0);
}

/// pi's terminalAppearance: white text contrasting at least as well as black = dark; a
/// clearly lighter foreground decides too.
#[test]
fn terminal_appearance_follows_pi() {
    use oxa_tui::{Appearance, TerminalColors};
    let bg = |rgb| TerminalColors {
        fg: None,
        bg: Some(rgb),
    };
    assert_eq!(bg((0x1c, 0x1c, 0x1c)).appearance(), Some(Appearance::Dark));
    assert_eq!(bg((0xff, 0xff, 0xff)).appearance(), Some(Appearance::Light));
    // Mid grey 0x80: black text contrasts better (luminance 0.216 > 0.179), so light...
    assert_eq!(bg((0x80, 0x80, 0x80)).appearance(), Some(Appearance::Light));
    // A white foreground on it is lighter, but white text on mid grey is below WCAG 4.5:
    // the contrast rule decides (light).
    let grey_with_white = TerminalColors {
        fg: Some((255, 255, 255)),
        bg: Some((0x80, 0x80, 0x80)),
    };
    assert_eq!(grey_with_white.appearance(), Some(Appearance::Light));
    // A dark blue background with light text: dark.
    let blue = TerminalColors {
        fg: Some((0xdd, 0xdd, 0xdd)),
        bg: Some((0x00, 0x00, 0x60)),
    };
    assert_eq!(blue.appearance(), Some(Appearance::Dark));
    assert_eq!(TerminalColors::default().appearance(), None);
}
