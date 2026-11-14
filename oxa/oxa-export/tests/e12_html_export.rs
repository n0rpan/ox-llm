//! E12 acceptance tests: HTML export. Owner: task E12.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_export::{
    ansi_lines_to_html, base64_encode, default_output_name, escape_html, export_html, format_for,
    session_data_json, strip_ansi_escapes, theme_css, write_export, ExportFormat, ExportOpts, ExportTheme,
    RenderedTool, CSS_PLACEHOLDERS, HTML_PLACEHOLDERS,
};
use oxa_fake::TestEnv;
use oxa_proto::{codes, shown_why, Entry, EntryBody, Mode, Role, ToolCall, Value, WireMsg};
use oxa_session::{encode_entry, SessionHeader};
use std::path::{Path, PathBuf};

fn header() -> SessionHeader {
    SessionHeader {
        version: 3,
        id: "sess-fixture".to_string(),
        ts_ms: 1_767_000_000_000,
        cwd: PathBuf::from("/work/project"),
        parent_session: None,
    }
}

fn entry(id: &str, parent: Option<&str>, n: u64, body: EntryBody) -> Entry {
    Entry {
        id: id.to_string(),
        parent: parent.map(str::to_string),
        ts_ms: 1_767_000_000_000 + n * 1000,
        body,
    }
}

fn msg(wire: WireMsg) -> EntryBody {
    EntryBody::Msg {
        wire,
        parts: Vec::new(),
    }
}

/// A small session: user, assistant with a built-in and a custom tool call, their results.
fn fixture() -> Vec<Entry> {
    let assistant = WireMsg {
        role: Role::Assistant,
        content: Some("Let me look.".to_string()),
        reasoning: None,
        tool_calls: vec![ToolCall {
            id: "call_1".to_string(),
            name: "read".to_string(),
            args_raw: "{\"path\":\"a.txt\"}".to_string(),
        }],
        tool_call_id: None,
    };
    let custom = WireMsg {
        role: Role::Assistant,
        content: Some("Running mytool.".to_string()),
        reasoning: None,
        tool_calls: vec![ToolCall {
            id: "call_2".to_string(),
            name: "mytool".to_string(),
            args_raw: "{}".to_string(),
        }],
        tool_call_id: None,
    };
    vec![
        entry("e0000001", None, 1, msg(WireMsg::user("Read a.txt, please."))),
        entry("e0000002", Some("e0000001"), 2, msg(assistant)),
        entry(
            "e0000003",
            Some("e0000002"),
            3,
            msg(WireMsg::tool_result("call_1", "hello from a.txt")),
        ),
        entry("e0000004", Some("e0000003"), 4, msg(custom)),
        entry(
            "e0000005",
            Some("e0000004"),
            5,
            msg(WireMsg::tool_result("call_2", "done")),
        ),
        entry(
            "e0000006",
            Some("e0000005"),
            6,
            msg(WireMsg::assistant("All read.")),
        ),
    ]
}

fn session_value(html: &str) -> Value {
    let open = "<script id=\"session-data\" type=\"application/json\">";
    let start = html.find(open).expect("the session data element") + open.len();
    let end = start + html[start..].find("</script>").expect("the element closes");
    let bytes = decode_base64(&html[start..end]);
    serde_json::from_slice(&bytes).expect("the session data is JSON")
}

/// Test-side base64 decoder (standard alphabet, padded), independent of the crate's encoder.
fn decode_base64(text: &str) -> Vec<u8> {
    let val = |c: u8| -> u32 {
        match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("not base64: {c}"),
        }
    };
    let bytes: Vec<u8> = text.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c) << (18 - 6 * i);
        }
        let produced = chunk.len() - 1;
        for k in 0..produced {
            out.push((n >> (16 - 8 * k)) as u8);
        }
    }
    out
}

/// golden HTML
#[test]
fn template_filled_from_fixture_session() {
    let entries = fixture();
    let opts = ExportOpts {
        session_name: Some("Fixture".to_string()),
        system_prompt: Some("You are a fixture.".to_string()),
        tools: serde_json::json!([{"name": "read", "description": "Read a file", "parameters": {}}]),
        ..ExportOpts::default()
    };
    let html = export_html(&header(), &entries, &opts).expect("the fixture exports");

    assert!(
        html.starts_with("<!DOCTYPE html>"),
        "a document: {}",
        &html[..40.min(html.len())]
    );
    assert!(html.contains("<title>Session Export</title>"));
    for p in CSS_PLACEHOLDERS.iter().chain(HTML_PLACEHOLDERS.iter()) {
        assert!(!html.contains(p), "placeholder {p} was not replaced");
    }
    // Dark defaults: the page colours of pi's dark theme.
    assert!(html.contains("--exportPageBg: #21252c;"), "dark page colour");
    assert!(html.contains("--body-bg: #21252c;"), "body background");

    let data = session_value(&html);
    assert_eq!(data["header"]["id"], "sess-fixture");
    assert_eq!(data["header"]["cwd"], "/work/project");
    assert_eq!(
        data["leafId"], "e0000006",
        "the last entry is the leaf by default"
    );
    assert_eq!(data["systemPrompt"], "You are a fixture.");
    assert_eq!(data["tools"][0]["name"], "read");
    let kept = data["entries"].as_array().expect("entries");
    assert_eq!(kept.len(), entries.len());
    assert_eq!(kept[0]["type"], "message");
    assert_eq!(kept[0]["message"]["role"], "user");
    assert_eq!(kept[0]["parentId"], Value::Null);
    assert_eq!(kept[1]["parentId"], "e0000001");
    assert!(
        data.get("renderedTools").is_none(),
        "no custom renderings, no key"
    );

    // The embedded bytes are the session file lines (pi v3 JSON).
    let mut line = Vec::new();
    encode_entry(&entries[1], &mut line);
    let json = session_data_json(&header(), &entries, &opts).expect("session data");
    let parsed: Value = serde_json::from_slice(&json).expect("JSON");
    assert_eq!(
        parsed["entries"][1],
        serde_json::from_slice::<Value>(&line).expect("line")
    );
    assert_eq!(decode_base64(&base64_encode(&json)), json, "base64 round trip");
}

/// per-tool blocks
#[test]
fn tool_renderers() {
    let entries = fixture();
    let mut opts = ExportOpts {
        rendered_tools: vec![
            RenderedTool {
                tool_call_id: "call_2".to_string(),
                call_html: Some(ansi_lines_to_html(&["\x1b[1mmytool\x1b[0m {}".to_string()])),
                result_collapsed_html: Some(ansi_lines_to_html(&["done".to_string()])),
                result_expanded_html: None,
            },
            RenderedTool {
                tool_call_id: "nothing".to_string(),
                call_html: None,
                result_collapsed_html: None,
                result_expanded_html: None,
            },
        ],
        ..ExportOpts::default()
    };
    let html = export_html(&header(), &entries, &opts).expect("exports");
    let data = session_value(&html);
    let rendered = &data["renderedTools"];
    assert_eq!(
        rendered["call_2"]["callHtml"],
        "<div class=\"ansi-line\"><span style=\"font-weight:bold\">mytool</span> {}</div>"
    );
    assert_eq!(
        rendered["call_2"]["resultHtmlCollapsed"],
        "<div class=\"ansi-line\">done</div>"
    );
    assert!(
        rendered["call_2"].get("resultHtmlExpanded").is_none(),
        "unset fields are left out"
    );
    assert!(
        rendered.get("nothing").is_none(),
        "a rendering with no HTML is not embedded"
    );
    assert!(
        rendered.get("call_1").is_none(),
        "built-in tools are drawn by the template"
    );

    // A custom tool's HTML is data: it never reaches the page as markup.
    opts.rendered_tools = vec![RenderedTool {
        tool_call_id: "call_2".to_string(),
        call_html: Some("<img src=x onerror=alert(1)>".to_string()),
        result_collapsed_html: None,
        result_expanded_html: None,
    }];
    let hostile = export_html(&header(), &entries, &opts).expect("exports");
    assert!(
        !hostile.contains("<img src=x onerror"),
        "embedded as base64 data only"
    );
}

/// colours and reset
#[test]
fn ansi_to_html() {
    assert_eq!(
        oxa_export::ansi_to_html("\x1b[31mred\x1b[0m plain"),
        "<span style=\"color:#800000\">red</span> plain"
    );
    assert_eq!(
        oxa_export::ansi_to_html("\x1b[92mg\x1b[0m"),
        "<span style=\"color:#00ff00\">g</span>"
    );
    assert_eq!(
        oxa_export::ansi_to_html("\x1b[1;38;5;196mX\x1b[0m"),
        "<span style=\"color:#ff0000;font-weight:bold\">X</span>"
    );
    assert_eq!(
        oxa_export::ansi_to_html("\x1b[48;2;1;2;3mY\x1b[39mZ\x1b[0m"),
        "<span style=\"background-color:rgb(1,2,3)\">Y</span><span style=\"background-color:rgb(1,2,3)\">Z</span>"
    );
    assert_eq!(
        oxa_export::ansi_to_html("\x1b[2;3;4mq\x1b[0m"),
        "<span style=\"opacity:0.6;font-style:italic;text-decoration:underline\">q</span>"
    );
    // Grey ramp and cube of 256 colours.
    assert_eq!(
        oxa_export::ansi_to_html("\x1b[38;5;232mk\x1b[0m"),
        "<span style=\"color:#080808\">k</span>"
    );
    assert_eq!(
        oxa_export::ansi_to_html("\x1b[38;5;16mc\x1b[0m"),
        "<span style=\"color:#000000\">c</span>"
    );
    // Unknown codes and CSI sequences are dropped; a stray ESC is removed.
    assert_eq!(oxa_export::ansi_to_html("\x1b[5mA\x1b[0m"), "A");
    assert_eq!(oxa_export::ansi_to_html("\x1b[?25lB"), "B");
    assert_eq!(oxa_export::ansi_to_html("a\x1bb"), "ab");
    // Reset clears everything; text after it is plain.
    assert_eq!(
        oxa_export::ansi_to_html("\x1b[1mbold\x1b[22m not\x1b[0m"),
        "<span style=\"font-weight:bold\">bold</span> not"
    );
    // Lines: an empty line is a non-breaking space.
    assert_eq!(
        ansi_lines_to_html(&["\x1b[1mb\x1b[0m".to_string(), String::new()]),
        "<div class=\"ansi-line\"><span style=\"font-weight:bold\">b</span></div><div class=\"ansi-line\">&nbsp;</div>"
    );
    // OSC 8 hyperlinks: http(s) and mailto only, URL escaped, controls stripped.
    assert_eq!(
        oxa_export::ansi_to_html("\x1b]8;;https://example.com/a?b=1&c\x07link\x1b]8;;\x07"),
        "<a href=\"https://example.com/a?b=1&amp;c\">link</a>"
    );
    assert_eq!(
        oxa_export::ansi_to_html("\x1b]8;;javascript:alert(1)\x07x\x1b]8;;\x07"),
        "x"
    );
    assert_eq!(
        strip_ansi_escapes("\x1b[31mred\x1b]8;;https://x\x07\x1b[0m"),
        "red"
    );
    assert_eq!(strip_ansi_escapes("a\x1b_apc\x1b\\b\x1bc"), "abc");
}

/// from the session file name
#[test]
fn derived_output_name() {
    let cwd = Path::new("/work");
    let file = Path::new("/home/u/.oxa/agent/sessions/--work--/2026-11-13T10-00-00-000Z_abc.jsonl");
    assert_eq!(
        default_output_name(Some(file), "abc", cwd),
        PathBuf::from("/work/oxa-session-2026-11-13T10-00-00-000Z_abc.html")
    );
    assert_eq!(
        default_output_name(None, "ephemeral-id", cwd),
        PathBuf::from("/work/oxa-session-ephemeral-id.html")
    );
    assert_eq!(format_for(Path::new("out.jsonl")), ExportFormat::Jsonl);
    assert_eq!(format_for(Path::new("out.JSONL")), ExportFormat::Jsonl);
    assert_eq!(format_for(Path::new("out.html")), ExportFormat::Html);
    assert_eq!(format_for(Path::new("out")), ExportFormat::Html);
}

/// content cannot inject markup
#[test]
fn html_escaping() {
    assert_eq!(
        escape_html("<a href='x'>&\"</a>"),
        "&lt;a href=&#039;x&#039;&gt;&amp;&quot;&lt;/a&gt;"
    );
    let mut entries = fixture();
    entries[0] = entry(
        "e0000001",
        None,
        1,
        msg(WireMsg::user("</script><script>alert(1)</script>")),
    );
    let html = export_html(&header(), &entries, &ExportOpts::default()).expect("exports");
    assert!(
        !html.contains("</script><script>alert"),
        "user text never appears as markup"
    );
    let data = session_value(&html);
    assert_eq!(
        data["entries"][0]["message"]["content"], "</script><script>alert(1)</script>",
        "the text survives as data"
    );
}

/// /export .jsonl copies the session
#[test]
fn jsonl_export() {
    let env = TestEnv::new();
    let session = env.write("sessions/s.jsonl", "{\"type\":\"session\"}\n");
    let copy = env.work().join("copy.jsonl");
    assert_eq!(format_for(&copy), ExportFormat::Jsonl);
    std::fs::copy(&session, &copy).expect("copy");
    assert_eq!(
        std::fs::read(&copy).expect("read"),
        std::fs::read(&session).expect("read"),
        "the JSONL export is a byte copy of the session file"
    );
    let html_target = env.work().join("out.html");
    assert_eq!(format_for(&html_target), ExportFormat::Html);
    let written = write_export(&html_target, "<!DOCTYPE html>").expect("writes");
    assert!(written.is_absolute());
    assert_eq!(
        std::fs::read_to_string(&written).expect("read"),
        "<!DOCTYPE html>"
    );
    let missing = env.work().join("no-such-dir").join("out.html");
    let err = write_export(&missing, "x").expect_err("parent directories are not created");
    assert_eq!(err.code, codes::IO);
}

/// shown / mode_change entries are left out of the embedded session data (pi's template.js is not edited)
#[test]
fn oxa_only_entries_filtered() {
    let entries = vec![
        entry("e0000001", None, 1, msg(WireMsg::user("hi"))),
        entry(
            "e0000002",
            Some("e0000001"),
            2,
            EntryBody::Shown {
                text: "aborted".to_string(),
                why: shown_why::ABORTED,
            },
        ),
        entry("e0000003", Some("e0000002"), 3, msg(WireMsg::assistant("hello"))),
        entry("e0000004", Some("e0000003"), 4, EntryBody::ModeChange(Mode::Plan)),
        entry("e0000005", Some("e0000004"), 5, msg(WireMsg::user("again"))),
    ];
    let opts = ExportOpts::default();
    let json = session_data_json(&header(), &entries, &opts).expect("session data");
    let data: Value = serde_json::from_slice(&json).expect("JSON");
    let kept = data["entries"].as_array().expect("entries");
    let ids: Vec<&str> = kept.iter().map(|e| e["id"].as_str().expect("id")).collect();
    assert_eq!(ids, ["e0000001", "e0000003", "e0000005"]);
    for e in kept {
        assert!(
            e["type"] != "shown" && e["type"] != "mode_change",
            "oxa-only type left out: {e}"
        );
    }
    // The tree stays connected: children of the dropped entries hang from the nearest kept one.
    assert_eq!(kept[1]["parentId"], "e0000001");
    assert_eq!(kept[2]["parentId"], "e0000003");
    // Kept entries keep the session file bytes.
    let mut line = Vec::new();
    encode_entry(&entries[0], &mut line);
    assert_eq!(serde_json::from_slice::<Value>(&line).expect("line"), kept[0]);
    // The default leaf is the last entry; a leaf on a dropped entry moves up to a kept one.
    assert_eq!(data["leafId"], "e0000005");
    let opts = ExportOpts {
        leaf: Some("e0000002".to_string()),
        ..ExportOpts::default()
    };
    let json = session_data_json(&header(), &entries, &opts).expect("session data");
    let data: Value = serde_json::from_slice(&json).expect("JSON");
    assert_eq!(data["leafId"], "e0000001");
}

#[test]
fn invalid_input_is_refused() {
    let err = export_html(&header(), &[], &ExportOpts::default()).expect_err("no entries");
    assert_eq!(err.code, codes::INVALID);
    let opts = ExportOpts {
        leaf: Some("ffffffff".to_string()),
        ..ExportOpts::default()
    };
    let err = export_html(&header(), &fixture(), &opts).expect_err("unknown leaf");
    assert_eq!(err.code, codes::INVALID);
}

#[test]
fn theme_colours_derive_from_the_user_message_background() {
    // A dark base: the page is darker, the card a little lighter than the page.
    let dark = ExportTheme {
        name: "custom".to_string(),
        vars: vec![("userMessageBg".to_string(), "#202020".to_string())],
        page_bg: None,
        card_bg: None,
        info_bg: None,
    };
    let (vars, body, container, info) = theme_css(&dark);
    assert!(vars.contains("--userMessageBg: #202020;"));
    assert!(vars.ends_with("--exportInfoBg: rgb(52, 47, 32);"), "{vars}");
    assert_eq!(body, "rgb(22, 22, 22)");
    assert_eq!(container, "rgb(27, 27, 27)");
    assert_eq!(info, "rgb(52, 47, 32)");

    // A light base: the page is slightly darker, the card is the base itself.
    let light = ExportTheme {
        vars: vec![("userMessageBg".to_string(), "rgb(240, 240, 240)".to_string())],
        ..dark.clone()
    };
    let (_, body, container, info) = theme_css(&light);
    assert_eq!(body, "rgb(230, 230, 230)");
    assert_eq!(container, "rgb(240, 240, 240)");
    assert_eq!(info, "rgb(250, 245, 220)");

    // Explicit export colours win; an unparseable base falls back to pi's dark defaults.
    let explicit = ExportTheme {
        vars: vec![("userMessageBg".to_string(), "not a colour".to_string())],
        page_bg: Some("#111111".to_string()),
        card_bg: None,
        info_bg: None,
        ..dark
    };
    let (_, body, container, info) = theme_css(&explicit);
    assert_eq!(body, "#111111");
    assert_eq!(container, "rgb(30, 30, 36)");
    assert_eq!(info, "rgb(60, 55, 40)");

    // The built-in dark theme is what an empty theme means.
    let (vars, body, _, _) = theme_css(&ExportTheme::dark());
    assert_eq!(body, "#21252c");
    assert!(vars.contains("--text: #dee0e1;"));
}

#[test]
fn base64_vectors() {
    assert_eq!(base64_encode(b""), "");
    assert_eq!(base64_encode(b"M"), "TQ==");
    assert_eq!(base64_encode(b"Ma"), "TWE=");
    assert_eq!(base64_encode(b"Man"), "TWFu");
    assert_eq!(base64_encode(&[0xfb, 0xff]), "+/8=");
}
