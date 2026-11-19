//! #1542: `oxa models` lists the in-process catalog (native local aliases and configured
//! cloud routes) with no controller CLI on PATH, and a stray word is a usage error rather
//! than a prompt.
//!
//! Isolation: [`TestEnv`] for every file and directory, synthetic models and limits only.
use oxa_cli::{parse_args, run, Stdio, Subcommand};
use oxa_config::Env;
use oxa_fake::TestEnv;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The fixture catalog: one native local alias and one cloud route, no credentials.
const CATALOG: &str = include_str!("fixtures/models-catalog.json");

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

/// A PATH directory with no `oxc`, `local-ai`, `python` or `python3` in it.
fn empty_path(env: &TestEnv) -> PathBuf {
    let bin = env.tmp().join("bin-without-a-controller");
    std::fs::create_dir_all(&bin).expect("create PATH directory");
    assert!(
        std::fs::read_dir(&bin).unwrap().next().is_none(),
        "the fixture PATH must stay empty"
    );
    bin
}

/// Run `oxa <words>` in-process with the fixture settings and the empty PATH.
fn go(env: &TestEnv, words: &[&str]) -> (i32, String, String) {
    env.write("home/.oxa/agent/settings.json", CATALOG);
    let path = empty_path(env);
    let path = path.to_string_lossy().into_owned();
    let owned: Vec<(String, String)> = env
        .envs("http://127.0.0.1:9")
        .into_iter()
        .map(|(name, value)| {
            if name == "PATH" {
                ("PATH".to_string(), path.clone())
            } else {
                (name, value)
            }
        })
        .collect();
    let pairs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let cwd = env.work();
    let env = Env::from_pairs(&pairs);
    run_with(&env, &cwd, words)
}

fn run_with(env: &Env, cwd: &Path, words: &[&str]) -> (i32, String, String) {
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
    let argv: Vec<String> = words.iter().map(|word| word.to_string()).collect();
    let code = run(&argv, env, io);
    (code, out.text(), err.text())
}

/// A row of the table, found by model id.
fn row<'a>(out: &'a str, model: &str) -> &'a str {
    out.lines()
        .find(|line| line.split_whitespace().any(|cell| cell == model))
        .unwrap_or_else(|| panic!("no row for {model} in {out:?}"))
}

#[test]
fn lists_the_fixture_catalog_without_a_controller_cli_on_path() {
    let env = TestEnv::new();
    let (code, out, err) = go(&env, &["models"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(!err.contains("Error"), "stderr: {err}");
    let header = out.lines().next().expect("header");
    for column in [
        "where", "model", "context", "max-out", "thinking", "images", "default",
    ] {
        assert!(header.contains(column), "header has {column}: {header}");
    }
    let local = row(&out, "fixture-local");
    assert!(local.starts_with("local"), "local row: {local}");
    assert!(local.contains("32K"), "local context: {local}");
    assert!(local.contains("4K"), "local max-out: {local}");
    assert!(local.ends_with("no"), "the cloud route is the default: {local}");
    assert!(!local.ends_with(' '), "no trailing spaces: {local}");
    let cloud = row(&out, "fixture-cloud");
    assert!(cloud.starts_with("cloud · anthropic"), "cloud row: {cloud}");
    assert!(cloud.contains("200K"), "cloud context: {cloud}");
    assert!(cloud.contains("8K"), "cloud max-out: {cloud}");
    assert!(cloud.ends_with("yes"), "the cloud route is the default: {cloud}");
}

#[test]
fn json_reports_the_endpoint_default_and_queue() {
    let env = TestEnv::new();
    let (code, out, err) = go(&env, &["models", "--json"]);
    assert_eq!(code, 0, "stderr: {err}");
    let document: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON document");
    assert_eq!(document["default"], "fixture-cloud");
    assert_eq!(document["queue"], 0);
    assert_eq!(document["count"], 2);
    let models = document["models"].as_array().expect("models array");
    let cloud = models
        .iter()
        .find(|model| model["model"] == "fixture-cloud")
        .expect("cloud row");
    assert_eq!(cloud["where"], "cloud · anthropic");
    assert_eq!(cloud["remote"], true);
    assert_eq!(cloud["context_window"], 200_000);
    assert_eq!(cloud["max_output"], 8_192);
    assert_eq!(cloud["default"], true);
    let local = models
        .iter()
        .find(|model| model["model"] == "fixture-local")
        .expect("local row");
    assert_eq!(local["where"], "local");
    assert_eq!(local["remote"], false);
    assert_eq!(local["context_window"], 32_768);
    assert_eq!(local["thinking"], true);
    assert_eq!(local["images"], false);
    assert_eq!(local["default"], false);
}

#[test]
fn search_filters_by_model_and_place() {
    let env = TestEnv::new();
    let (code, out, err) = go(&env, &["models", "ANTHROPIC"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert_eq!(out.lines().count(), 2, "header and the cloud row: {out:?}");
    assert!(out.contains("fixture-cloud"), "{out:?}");
    assert!(!out.contains("fixture-local"), "{out:?}");
    let (code, out, _) = go(&env, &["models", "no-such-model"]);
    assert_eq!(code, 0);
    assert_eq!(out.trim(), "No models matching \"no-such-model\"");
}

/// A known command must never fall back to being sent as a prompt, and a stray word must be a
/// usage error (exit 2) rather than a message to the model.
#[test]
fn models_is_a_command_and_a_stray_word_is_a_usage_error() {
    assert_eq!(
        parse_args(&["models".to_string()], &[]).unwrap().subcommand,
        Some(Subcommand::Models(Vec::new()))
    );
    // A leading `--offline` keeps the catalog a command too, so its offline cloud refusal
    // can be reported instead of the word becoming a prompt.
    assert_eq!(
        parse_args(&["--offline".to_string(), "models".to_string()], &[])
            .unwrap()
            .subcommand,
        Some(Subcommand::Models(Vec::new()))
    );
    let env = TestEnv::new();
    let (code, out, err) = go(&env, &["models", "--no-such-flag"]);
    assert_eq!(code, 2, "usage error, not a prompt: {err}");
    assert!(out.is_empty(), "nothing on stdout: {out:?}");
    assert!(err.contains("unexpected argument"), "stderr: {err}");
    assert!(err.contains("oxa models"), "usage hint: {err}");
}
