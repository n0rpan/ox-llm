//! Task 61 part A: shared, panic-free entry points for the oxe fuzz targets.
//!
//! No model weights are needed: the body/HTTP normalisers and the chat parser are pure, and the
//! template is the committed Qwen fixture. `tests/smoke.rs` runs the same functions in normal CI.
#![forbid(unsafe_code)]

use chat::parser::{self, ParserConfig};
use chat::request::{self, ServerChatOptions};
use chat::Session;
use chat::{ChatTemplate, Format, ReasoningFormat, Tool, ToolChoice};
use http::{HeaderMap, HeaderValue};
use serde_json::Value;

pub const TEMPLATE: &str =
    include_str!("../../../engine/crates/chat/src/fixtures/qwen38-flash-next.jinja");
pub const GEN_PROMPT: &str = "<|im_start|>assistant\n";

pub fn template() -> ChatTemplate {
    ChatTemplate::new(TEMPLATE, "<|im_start|>", "<|im_end|>").expect("fixture template")
}

pub fn parser_config() -> ParserConfig {
    ParserConfig {
        format: Format::Qwen3Coder,
        tools: vec![Tool {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        }],
        tool_choice: ToolChoice::Auto,
        parallel_tool_calls: true,
        reasoning_format: ReasoningFormat::Deepseek,
        supports_thinking: true,
        lenient: true,
    }
}

/// Arbitrary bytes as a JSON request body: invalid UTF-8/JSON is a 400 by contract, not a panic.
pub fn chat_request(data: &[u8]) {
    let Ok(body) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    let template = template();
    let opts = ServerChatOptions::default();
    if let Ok(inputs) = request::chat_inputs(&body, &opts, &template) {
        let _ = request::apply(&template, &inputs);
        let _ = request::template_marks(&template, &inputs, "x");
    }
}

pub fn chat_parser(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let cfg = parser_config();
    let _ = parser::parse(&cfg, GEN_PROMPT, &text, true);
    let _ = parser::parse(&cfg, GEN_PROMPT, &text, false);

    // Streaming: every chunk size, incremental and reference mode. A parse error is data.
    let mut session = Session::new(cfg, GEN_PROMPT.to_string(), Box::new(|| "id".to_string()));
    for chunk in text.as_bytes().chunks(7) {
        let _ = session.feed(&String::from_utf8_lossy(chunk), false);
    }
    let _ = session.feed("", true);
}

pub fn chat_session(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let cfg = parser_config();
    let mut fast = Session::new(
        cfg.clone(),
        GEN_PROMPT.to_string(),
        Box::new(|| "id".to_string()),
    );
    let mut slow = Session::new(cfg, GEN_PROMPT.to_string(), Box::new(|| "id".to_string()));
    slow.incremental = false;
    for chunk in text.as_bytes().chunks(11) {
        let a = fast.feed(&String::from_utf8_lossy(chunk), false);
        let b = slow.feed(&String::from_utf8_lossy(chunk), false);
        assert_eq!(a.is_ok(), b.is_ok(), "incremental/reference error mismatch");
        assert_eq!(
            fast.msg(),
            slow.msg(),
            "incremental/reference message mismatch"
        );
    }
    let a = fast.feed("", true);
    let b = slow.feed("", true);
    assert_eq!(
        a.is_ok(),
        b.is_ok(),
        "incremental/reference final error mismatch"
    );
    assert_eq!(
        fast.msg(),
        slow.msg(),
        "incremental/reference final message mismatch"
    );
}

pub fn api_params(data: &[u8]) {
    let Ok(body) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    let _ = server::api::gen_params(&body, true);
    let _ = server::api::gen_params(&body, false);
    let _ = server::api::check_sampling(&body);
    let _ = server::api::logit_bias(&body);
    let _ = server::api::has_length_field(&body);
}

pub fn http_access(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let _ = server::http::parse_key_file(&text);

    let access = server::http::Access {
        port: 8093,
        extra_hosts: vec!["gpu.example.invalid".into()],
        keys: vec![b"k".to_vec()],
    };
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("host", text.as_bytes()),
        ("origin", text.as_bytes()),
        ("sec-fetch-site", text.as_bytes()),
        ("authorization", text.as_bytes()),
        ("x-oxe-state-policy", text.as_bytes()),
        ("x-oxe-snapshot", text.as_bytes()),
    ] {
        if let Ok(value) = HeaderValue::from_bytes(value) {
            headers.append(name, value);
        }
    }
    let _ = access.check("POST", "/v1/chat/completions", &headers);
    let _ = server::http::state_policy(&headers);
    let _ = server::http::snapshot_keep(&headers);
}

pub fn corpus(name: &str) -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("corpus")
            .join(name),
    )
    .unwrap_or_else(|e| panic!("read corpus {name}: {e}"))
}
