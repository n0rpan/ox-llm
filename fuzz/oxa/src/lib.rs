//! Task 61 part A: shared, panic-free entry points for the oxa fuzz targets.
//!
//! The bins are thin `libfuzzer_sys` wrappers; `tests/smoke.rs` runs the same functions over a
//! committed corpus in normal CI, so a regression fails the build without a nightly toolchain.
#![forbid(unsafe_code)]

use oxa_core::repair::{self, Declared};
use oxa_engine::{ReplyAssembler, SseEvent, SseParser};
use oxa_proto::{AssistantTurn, Finish, ThinkingLevel, Usage, WireMsg};
use oxa_tui::{render_markdown, visible_width, MarkdownOpts, Theme};
use std::path::Path;

pub fn sse(data: &[u8]) {
    let mut all_at_once = SseParser::new();
    let whole = all_at_once.feed(data);

    // The documented contract: correct for every split of the byte stream.
    let mut byte_at_a_time = SseParser::new();
    let mut split = Vec::new();
    for byte in data {
        split.extend(byte_at_a_time.feed(std::slice::from_ref(byte)));
    }
    assert_eq!(whole, split, "SSE parser is not split-invariant");
}

pub fn reply(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let mut assembler = ReplyAssembler::new();
    for line in text.split('\n') {
        // Both entry points take untrusted engine text. Malformed JSON/values are errors, not
        // panics; the fuzzer only requires that no error escapes as a panic.
        let _ = assembler.chunk(line);
        let _ = assembler.comment(line, 7);
    }
    let _ = assembler.finish(ThinkingLevel::Off);
}

pub fn repair(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let tools = [
        Declared {
            name: "read",
            schema_json: r#"{"properties":{"path":{"type":"string"},"limit":{"type":"integer"}},"required":["path"]}"#,
        },
        Declared {
            name: "bash",
            schema_json: r#"{"properties":{"command":{"type":"string"}},"required":["command"]}"#,
        },
    ];
    let _ = repair::find_opener(&text);
    if let Some(at) = repair::find_opener(&text) {
        let _ = repair::classify(&text[at..], &tools);
    }
    let turn = AssistantTurn {
        msg: WireMsg {
            images: Vec::new(),
            provider_history: None,
            role: oxa_proto::Role::Assistant,
            content: Some(text.to_string()),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
        },
        finish: Finish::Stop,
        usage: Usage {
            prompt: 0,
            completion: 0,
            cached: 0,
            cached_missing: true,
        },
        timings: None,
        model: String::new(),
        level: ThinkingLevel::Off,
    };
    let _ = repair::detect(&turn, &tools);
}

pub fn markdown(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let width = (data.first().copied().unwrap_or(79) as u16) % 160 + 1;
    let opts = MarkdownOpts {
        hyperlinks: data.get(1).is_some_and(|b| b & 1 == 1),
        padding_x: u16::from(data.get(2).copied().unwrap_or(0) % 4),
        ..MarkdownOpts::default()
    };
    let lines = render_markdown(&text, width, &Theme::plain(), &opts);
    for line in &lines {
        assert!(
            !line.contains('\n') && !line.contains('\r'),
            "line break in a rendered line"
        );
        assert!(
            visible_width(line) <= usize::from(width),
            "rendered line wider than the requested width"
        );
        if !opts.hyperlinks {
            // Theme::plain() emits no SGR/OSC: any control character here came from the text.
            assert!(
                !line.chars().any(char::is_control),
                "untrusted text reached the terminal raw"
            );
        }
    }
}

pub fn session(data: &[u8]) {
    let _ = oxa_session::decode_header(data);
    let _ = oxa_session::decode_entry(data);
}

pub fn mcp_sse(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let _ = oxa_mcp::parse_sse_messages(&text);
}

pub fn rpc(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let _ = oxa_rpc::parse_command(&text);
}

pub fn path(data: &[u8]) {
    let arg = String::from_utf8_lossy(data);
    let resolved = oxa_tools::resolve_path(Path::new("/workspace/project"), &arg);
    // The lexical resolver must never return a path whose components still contain `..`.
    assert!(!resolved
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir)));
}

/// The committed seeds, used by the short normal-CI smoke. Missing files are a test failure.
pub fn corpus(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("corpus")
            .join(name),
    )
    .unwrap_or_else(|e| panic!("read corpus {name}: {e}"))
}

pub fn sse_event_name(e: &SseEvent) -> &'static str {
    match e {
        SseEvent::Data(_) => "data",
        SseEvent::Comment(_) => "comment",
        SseEvent::Done => "done",
    }
}
