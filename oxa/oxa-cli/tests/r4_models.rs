//! R4 acceptance tests: `oxa models [--json]` (#1542), the listing the ox AI panel reads.
//! Isolation: `oxa_fake::TestEnv` for every file and directory; never the real home.

use oxa_cli::{parse_args, run, Stdio, Subcommand};
use oxa_config::Env;
use oxa_fake::{FakeOpts, FakeServer, TestEnv};
use serde_json::Value;
use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

const FIXTURE: &str = include_str!("../../omarchy-panel/tests/fixtures/models.json");

/// A byte sink the test can read back after `run` consumed the boxed writer.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Sink {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).expect("utf-8 output")
    }
}

struct Captured {
    code: i32,
    out: String,
    err: String,
}

fn env_of(env: &TestEnv, engine_url: &str) -> Env {
    let owned = env.envs(engine_url);
    let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    Env::from_pairs(&pairs)
}

fn go_with(env: &Env, cwd: &Path, words: &[&str]) -> Captured {
    let out = Sink::default();
    let err = Sink::default();
    let io = Stdio {
        stdin: Box::new(Cursor::new(Vec::new())),
        stdout: Box::new(out.clone()),
        stderr: Box::new(err.clone()),
        stdin_is_tty: false,
        stdout_is_tty: false,
        cwd: cwd.to_path_buf(),
    };
    let argv: Vec<String> = words.iter().map(|s| s.to_string()).collect();
    let code = run(&argv, env, io);
    Captured {
        code,
        out: out.text(),
        err: err.text(),
    }
}

fn keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().expect("object").keys().cloned().collect();
    keys.sort();
    keys
}

/// `models` is a known word, so it never becomes a model prompt (#1542).
#[test]
fn oxa_models_is_not_a_prompt() {
    let argv: Vec<String> = ["models", "--json"].iter().map(|s| s.to_string()).collect();
    let args = parse_args(&argv, &[]).expect("models parses");
    assert_eq!(
        args.subcommand,
        Some(Subcommand::Models(vec!["--json".to_string()]))
    );
    assert!(args.messages.is_empty(), "no prompt words: {:?}", args.messages);
}

/// The JSON of the engine's own models has the keys of the frozen fixture row, and the object
/// has its type and version.
#[test]
fn models_json_from_engine_matches_fixture_shape() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        Vec::new(),
        FakeOpts {
            n_ctx: 32_000,
            model: "fake-model".to_string(),
            ..FakeOpts::default()
        },
    )
    .expect("fake engine");
    let captured = go_with(&env_of(&env, &server.url()), &env.work(), &["models", "--json"]);
    assert_eq!(captured.code, 0, "stderr: {}", captured.err);
    let value: Value = serde_json::from_str(captured.out.trim()).expect("one JSON object");
    assert_eq!(value["type"], "oxa_models");
    assert_eq!(value["version"], 1);
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture parses");
    let rows = value["models"].as_array().expect("models array");
    assert!(!rows.is_empty(), "the fake engine lists its model");
    assert_eq!(
        keys(&rows[0]),
        keys(&fixture["models"][0]),
        "row keys match the fixture"
    );
    assert!(
        rows.iter().any(|row| row["id"] == "fake-model"),
        "the fake model is listed: {value}"
    );
}

/// Listing reads no key and no ledger: a fresh environment has neither, and the listing
/// succeeds with no error and no written output other than the JSON object.
#[test]
fn models_reads_no_key_and_no_ledger() {
    let env = TestEnv::new();
    let server = FakeServer::start(Vec::new(), FakeOpts::default()).expect("fake engine");
    let captured = go_with(&env_of(&env, &server.url()), &env.work(), &["models", "--json"]);
    assert_eq!(captured.code, 0, "stderr: {}", captured.err);
    let lines: Vec<&str> = captured.out.lines().collect();
    assert_eq!(lines.len(), 1, "one JSON line: {:?}", captured.out);
    let value: Value = serde_json::from_str(lines[0]).expect("JSON");
    assert_eq!(value["type"], "oxa_models");
}

/// A bad word is a usage error, not a prompt and not a listing.
#[test]
fn models_rejects_unknown_words() {
    let env = TestEnv::new();
    let captured = go_with(
        &env_of(&env, "http://127.0.0.1:9"),
        &env.work(),
        &["models", "--frobnicate"],
    );
    assert_eq!(
        captured.code, 2,
        "stdout: {} stderr: {}",
        captured.out, captured.err
    );
    assert!(captured.out.is_empty(), "no listing: {}", captured.out);
}
