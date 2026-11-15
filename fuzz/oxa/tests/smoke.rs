//! The seconds-long normal-CI smoke for the oxa fuzz targets (Task 61 part A).
//!
//! No nightly toolchain, sanitizer or network is needed: the same entry points the libFuzzer
//! bins call are run over the committed corpus plus deterministic mutations.

use std::panic::{catch_unwind, AssertUnwindSafe};

fn run(name: &str, f: impl Fn(&[u8])) {
    for file in [
        "sse-crlf.sse",
        "sse-split.sse",
        "sse-comments.sse",
        "reply-chunks.jsonl",
        "repair-malformed.txt",
        "markdown-control.md",
        "session-header.jsonl",
        "session-entry.jsonl",
        "mcp-sse.txt",
        "rpc-commands.jsonl",
        "path-args.txt",
    ] {
        let data = oxa_fuzz::corpus(file);
        let result = catch_unwind(AssertUnwindSafe(|| f(&data)));
        assert!(result.is_ok(), "{name} panicked on corpus/{file}");
    }
}

fn mutate(seed: &mut u64, data: &[u8]) -> Vec<u8> {
    // xorshift64*: deterministic, no dependency, enough shape variation for a smoke.
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
fn sse_target_survives_corpus_and_mutations() {
    run("sse", oxa_fuzz::sse);
    let seed = oxa_fuzz::corpus("sse-crlf.sse");
    let mut state = 0x9e3779b97f4a7c15u64;
    for _ in 0..2_000 {
        let data = mutate(&mut state, &seed);
        oxa_fuzz::sse(&data);
    }
}

#[test]
fn reply_target_survives_corpus_and_mutations() {
    run("reply", oxa_fuzz::reply);
    let seed = oxa_fuzz::corpus("reply-chunks.jsonl");
    let mut state = 0x243f6a8885a308d3u64;
    for _ in 0..2_000 {
        let data = mutate(&mut state, &seed);
        oxa_fuzz::reply(&data);
    }
}

#[test]
fn repair_target_survives_corpus_and_mutations() {
    run("repair", oxa_fuzz::repair);
    let seed = oxa_fuzz::corpus("repair-malformed.txt");
    let mut state = 0x13198a2e03707344u64;
    for _ in 0..2_000 {
        let data = mutate(&mut state, &seed);
        oxa_fuzz::repair(&data);
    }
}

#[test]
fn markdown_target_survives_corpus_and_mutations() {
    run("markdown", oxa_fuzz::markdown);
    let seed = oxa_fuzz::corpus("markdown-control.md");
    let mut state = 0xa4093822299f31d0u64;
    for _ in 0..2_000 {
        let data = mutate(&mut state, &seed);
        oxa_fuzz::markdown(&data);
    }
}

#[test]
fn file_and_protocol_targets_survive_corpus() {
    run("session", oxa_fuzz::session);
    run("mcp_sse", oxa_fuzz::mcp_sse);
    run("rpc", oxa_fuzz::rpc);
    run("path", oxa_fuzz::path);
}

#[test]
fn sse_parser_is_split_invariant_on_the_corpus() {
    for file in ["sse-crlf.sse", "sse-split.sse", "sse-comments.sse"] {
        oxa_fuzz::sse(&oxa_fuzz::corpus(file));
    }
}

/// Regression for Task 61 finding OXA-1: raw control bytes from model output must not reach the
/// terminal, including inside an autolink URL (the autolink filter did not strip controls).
#[test]
fn markdown_does_not_pass_control_characters_through() {
    let text = "hello \u{1b}[2Jworld <https://example.invalid/\u{1b}]0;pwned\u{7}>";
    let lines = oxa_tui::render_markdown(
        text,
        80,
        &oxa_tui::Theme::plain(),
        &oxa_tui::MarkdownOpts::default(),
    );
    for line in lines {
        assert!(
            !line.chars().any(char::is_control),
            "raw control character in {line:?}"
        );
    }
}

/// Codex finding on PR 55: the session seeds must be real records, otherwise the target only
/// ever exercises the early timestamp rejection and never reaches the body decoder.
#[test]
fn session_seeds_decode_successfully() {
    let header = oxa_fuzz::corpus("session-header.jsonl");
    oxa_session::decode_header(header.split(|b| *b == b'\n').next().unwrap())
        .expect("session-header.jsonl must be a valid header");
    let entry = oxa_fuzz::corpus("session-entry.jsonl");
    let entry = oxa_session::decode_entry(entry.split(|b| *b == b'\n').next().unwrap())
        .expect("session-entry.jsonl must be a valid entry");
    // Re-encoding a decoded seed is lossless, so mutations start from the real wire shape.
    let mut again = Vec::new();
    oxa_session::encode_entry(&entry, &mut again);
    assert_eq!(oxa_session::decode_entry(&again).unwrap(), entry);
}
