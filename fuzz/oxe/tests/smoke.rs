//! The seconds-long normal-CI smoke for the oxe fuzz targets (Task 61 part A).
//!
//! No model weights, network or nightly toolchain: the pure HTTP/JSON normalisers and the chat
//! parser/template are driven over a committed corpus plus deterministic mutations.

use std::panic::{catch_unwind, AssertUnwindSafe};

const FILES: [&str; 6] = [
    "chat-request.json",
    "chat-tool-calls.txt",
    "chat-reasoning.txt",
    "api-params.json",
    "http-values.txt",
    "key-file.txt",
];

fn run(name: &str, f: impl Fn(&[u8])) {
    for file in FILES {
        let data = oxe_fuzz::corpus(file);
        let result = catch_unwind(AssertUnwindSafe(|| f(&data)));
        assert!(result.is_ok(), "{name} panicked on corpus/{file}");
    }
}

fn mutate(seed: &mut u64, data: &[u8]) -> Vec<u8> {
    *seed ^= *seed >> 12;
    *seed ^= *seed << 25;
    *seed ^= *seed >> 27;
    let mut out = data.to_vec();
    for _ in 0..(*seed % 6) {
        let at = (*seed as usize) % (out.len() + 1);
        *seed ^= *seed >> 12;
        *seed ^= *seed << 25;
        *seed ^= *seed >> 27;
        match *seed % 4 {
            0 if at < out.len() => out[at] = (*seed & 0xff) as u8,
            1 => out.insert(at, (*seed & 0xff) as u8),
            2 if at < out.len() => {
                out.remove(at);
            }
            _ => out.truncate(at),
        }
    }
    out
}

#[test]
fn every_target_survives_the_corpus_and_mutations() {
    for (name, f) in [
        ("chat_request", oxe_fuzz::chat_request as fn(&[u8])),
        ("chat_parser", oxe_fuzz::chat_parser),
        ("chat_session", oxe_fuzz::chat_session),
        ("api_params", oxe_fuzz::api_params),
        ("http_access", oxe_fuzz::http_access),
    ] {
        run(name, f);
        for file in FILES {
            let seed = oxe_fuzz::corpus(file);
            let mut state = 0x9e3779b97f4a7c15u64;
            for _ in 0..300 {
                let data = mutate(&mut state, &seed);
                let result = catch_unwind(AssertUnwindSafe(|| f(&data)));
                assert!(result.is_ok(), "{name} panicked on a mutation of {file}");
            }
        }
    }
}

#[test]
fn chat_parser_handles_a_tool_call_and_reasoning() {
    let raw = oxe_fuzz::corpus("chat-tool-calls.txt");
    let text = std::str::from_utf8(&raw).unwrap();
    let cfg = oxe_fuzz::parser_config();
    let msg = chat::parser::parse(&cfg, oxe_fuzz::GEN_PROMPT, text, false).expect("fixture parses");
    assert_eq!(msg.tool_calls.len(), 1);
    assert_eq!(msg.tool_calls[0].name, "read");
    assert!(msg.reasoning_content.contains("think"));
}

#[test]
fn chat_template_renders_a_request_and_escapes_message_text() {
    let body: serde_json::Value =
        serde_json::from_slice(&oxe_fuzz::corpus("chat-request.json")).unwrap();
    let template = oxe_fuzz::template();
    let inputs = chat::request::chat_inputs(
        &body,
        &chat::request::ServerChatOptions::default(),
        &template,
    )
    .expect("fixture request is valid");
    let applied = chat::request::apply(&template, &inputs).expect("fixture renders");
    assert!(applied.prompt.contains("hi"));
    assert!(applied.generation_prompt.contains("assistant"));
}
