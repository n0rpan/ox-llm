//! D13 acceptance tests: Binary and dispatch. Owner: task D13.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_cli::{choose_mode, parse_args, run, RunMode, Stdio};
use oxa_config::Env;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_session::{FsStore, SessionStore};
use std::io::{Cursor, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

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

/// The complete environment of a child `oxa` (TestEnv::envs), as the `Env` that `run` takes.
fn env_of(env: &TestEnv, engine_url: &str) -> Env {
    let owned = env.envs(engine_url);
    let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    Env::from_pairs(&pairs)
}

/// Run the program in-process with the given streams. Stdin is a pipe (not a terminal) unless
/// `stdin_tty`, so the default mode is print.
fn go_with(
    env: &Env,
    cwd: &Path,
    words: &[&str],
    stdin: &str,
    stdin_tty: bool,
    stdout_tty: bool,
) -> Captured {
    let out = Sink::default();
    let err = Sink::default();
    let io = Stdio {
        stdin: Box::new(Cursor::new(stdin.as_bytes().to_vec())),
        stdout: Box::new(out.clone()),
        stderr: Box::new(err.clone()),
        stdin_is_tty: stdin_tty,
        stdout_is_tty: stdout_tty,
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

fn go(env: &TestEnv, engine_url: &str, words: &[&str]) -> Captured {
    go_with(&env_of(env, engine_url), &env.work(), words, "", false, false)
}

/// A URL where nothing listens: the port was free a moment ago and is connection-refused now.
fn closed_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|s| s.to_string()).collect()
}

/// table of cli.md
#[test]
fn mode_choice_by_tty_rules() {
    // (flags, stdin tty, stdout tty, expected)
    let cases: &[(&[&str], bool, bool, RunMode)] = &[
        (&[], true, true, RunMode::Interactive),
        (&[], false, true, RunMode::Print),
        (&[], true, false, RunMode::Print),
        (&[], false, false, RunMode::Print),
        (&["-p", "hi"], true, true, RunMode::Print),
        (&["--print"], true, true, RunMode::Print),
        (&["--mode", "text"], true, true, RunMode::Interactive),
        (&["--mode", "text", "-p", "hi"], true, true, RunMode::Print),
        (&["--mode", "json"], true, true, RunMode::Json),
        (&["--mode", "json"], false, false, RunMode::Json),
        (&["--mode", "rpc"], true, true, RunMode::Rpc),
        (&["--mode", "rpc"], false, false, RunMode::Rpc),
    ];
    for (flags, stdin_tty, stdout_tty, expected) in cases {
        let a = parse_args(&argv(flags), &[]).unwrap_or_else(|e| panic!("parse {flags:?}: {e}"));
        assert_eq!(
            choose_mode(&a, *stdin_tty, *stdout_tty),
            *expected,
            "flags {flags:?}, stdin tty {stdin_tty}, stdout tty {stdout_tty}"
        );
    }
}

/// 0 / 1 / 2
#[test]
fn exit_codes() {
    let env = TestEnv::new();
    let url = closed_url();
    assert_eq!(go(&env, &url, &["--help"]).code, 0, "--help");
    assert_eq!(go(&env, &url, &["--version"]).code, 0, "--version");
    assert_eq!(
        go(&env, &url, &["--mode", "rpc", "-p"]).code,
        2,
        "documented conflict is a usage error"
    );
    assert_eq!(go(&env, &url, &["--no-such-flag"]).code, 2, "unknown long flag");
    assert_eq!(go(&env, &url, &["-x"]).code, 2, "unknown short flag");
    assert_eq!(
        go(&env, &url, &["--export", "missing-session.jsonl"]).code,
        1,
        "runtime failure"
    );
}

/// message + hint
#[test]
fn no_engine_one_line_exit_2() {
    let env = TestEnv::new();
    let result = go(&env, &closed_url(), &["-p", "hello"]);
    assert_eq!(result.code, 2, "no engine is exit 2");
    let lines: Vec<&str> = result.err.lines().collect();
    assert_eq!(lines.len(), 1, "one line on stderr, got {:?}", result.err);
    assert!(
        lines[0].contains("cannot reach the engine"),
        "message: {}",
        lines[0]
    );
    assert!(lines[0].contains("OXA_ENGINE_URL"), "hint: {}", lines[0]);
    assert!(result.out.is_empty(), "nothing on stdout");
}

/// output format
#[test]
fn list_models() {
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
    let all = go(&env, &server.url(), &["--list-models"]);
    assert_eq!(all.code, 0, "stderr: {}", all.err);
    let lines: Vec<&str> = all.out.lines().collect();
    assert_eq!(lines.len(), 2, "header and one row, got {:?}", all.out);
    let header = lines[0];
    for column in ["provider", "model", "context", "max-out", "thinking", "images"] {
        assert!(header.contains(column), "header has {column}: {header}");
    }
    assert!(lines[1].starts_with("oxe"), "provider column first: {}", lines[1]);
    assert!(lines[1].contains("fake-model"), "model id: {}", lines[1]);
    assert!(lines[1].contains("32K"), "context in K: {}", lines[1]);
    assert!(!lines[1].ends_with(' '), "no trailing spaces");

    let hit = go(&env, &server.url(), &["--list-models", "FAKE"]);
    assert_eq!(hit.out.lines().count(), 2, "search is case-insensitive");
    let miss = go(&env, &server.url(), &["--list-models", "zzz"]);
    assert_eq!(miss.code, 0);
    assert_eq!(miss.out.trim(), "No models matching \"zzz\"");
}

/// --export input [output]
#[test]
fn export_flag() {
    let env = TestEnv::new();
    // Arguments and the failure path: an unreadable input is exit 1 with one error line.
    let missing = go(&env, &closed_url(), &["--export", "nope.jsonl", "out.html"]);
    assert_eq!(missing.code, 1);
    assert!(missing.err.contains("cannot read"), "stderr: {}", missing.err);
    assert!(missing.out.is_empty());
}

/// --export writes HTML and prints the path. The input is a session file written by a print
/// run against the fake engine.
#[test]
fn export_flag_writes_html() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![ScriptTurn::text("Export me.")], FakeOpts::default()).expect("fake");
    let first = go(&env, &server.url(), &["-p", "hello"]);
    assert_eq!(first.code, 0, "stderr: {}", first.err);
    let input = first_jsonl(&env.agent_dir().join("sessions")).expect("the print run wrote a session file");
    let result = go(
        &env,
        &closed_url(),
        &["--export", input.to_str().expect("utf-8 path"), "out.html"],
    );
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    assert!(result.out.starts_with("Exported to: "));
    let html = std::fs::read_to_string(env.work().join("out.html")).expect("the export file");
    assert!(html.starts_with("<!DOCTYPE html>") || html.starts_with("<!doctype html>"));
}

/// The first `.jsonl` file under `dir` (searched recursively, names sorted).
fn first_jsonl(dir: &Path) -> Option<std::path::PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if let Some(found) = first_jsonl(&path) {
                return Some(found);
            }
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            return Some(path);
        }
    }
    None
}

/// `help` output lists the options; `--version` prints the program version.
#[test]
fn help_and_version() {
    let env = TestEnv::new();
    let url = closed_url();
    let help = go(&env, &url, &["--help"]);
    assert_eq!(help.code, 0);
    assert!(help.out.starts_with("oxa - "), "title: {}", help.out);
    assert!(help.out.contains("Usage:"));
    assert!(help.out.contains("--print, -p"));
    assert!(help.err.is_empty());

    let version = go(&env, &url, &["-v"]);
    assert_eq!(version.code, 0);
    assert_eq!(version.out, format!("oxa {}\n", env!("CARGO_PKG_VERSION")));
}

/// copies, never overwrites
#[test]
fn config_import_pi_once() {
    let env = TestEnv::new();
    env.write("home/.pi/agent/mcp.json", "{\"servers\":{}}");
    env.write("home/.pi/agent/keybindings.json", "{\"from\":\"pi\"}");
    env.write(
        "home/.pi/agent/skills/demo/SKILL.md",
        "---\nname: demo\n---\nbody\n",
    );
    env.write("home/.pi/agent/prompts/review.md", "review\n");
    env.write("home/.oxa/agent/keybindings.json", "{\"from\":\"oxa\"}");
    let url = closed_url();

    let first = go(&env, &url, &["config", "--import-pi"]);
    assert_eq!(first.code, 0, "stderr: {}", first.err);
    assert!(first.out.contains("imported 3 file(s)"), "report: {}", first.out);
    assert!(first.out.contains("kept 1 existing"), "report: {}", first.out);
    let agent = env.agent_dir();
    assert_eq!(
        std::fs::read_to_string(agent.join("mcp.json")).unwrap(),
        "{\"servers\":{}}"
    );
    assert!(agent.join("skills/demo/SKILL.md").is_file(), "skills copied");
    assert!(agent.join("prompts/review.md").is_file(), "prompts copied");
    assert_eq!(
        std::fs::read_to_string(agent.join("keybindings.json")).unwrap(),
        "{\"from\":\"oxa\"}",
        "an existing file is never overwritten"
    );

    let second = go(&env, &url, &["config", "--import-pi"]);
    assert_eq!(second.code, 0);
    assert!(
        second.out.contains("imported 0 file(s)"),
        "second run copies nothing: {}",
        second.out
    );
    assert_eq!(
        std::fs::read_to_string(agent.join("keybindings.json")).unwrap(),
        "{\"from\":\"oxa\"}"
    );
}

/// `oxa -p` against oxa-fake
#[test]
fn print_mode_end_to_end_with_fake() {
    let env = TestEnv::new();
    let server =
        FakeServer::start(vec![ScriptTurn::text("pong from the fake")], FakeOpts::default()).expect("fake");
    let result = go_with(
        &env_of(&env, &server.url()),
        &env.work(),
        &["-p", "ping"],
        "",
        false,
        false,
    );
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    assert_eq!(
        result.out, "pong from the fake\n",
        "only the final text reaches stdout"
    );
    let sent = format!("{:?}", server.requests()[0].messages);
    assert!(sent.contains("ping"), "the prompt was sent: {sent}");
}

/// Piped stdin is prepended to the first prompt; `@file` text goes into the first prompt too.
#[test]
fn piped_stdin_and_file_arguments_form_the_first_prompt() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![ScriptTurn::text("first"), ScriptTurn::text("second")],
        FakeOpts::default(),
    )
    .expect("fake");
    env.write("work/notes.txt", "file body\n");
    let envv = env_of(&env, &server.url());
    let result = go_with(
        &envv,
        &env.work(),
        &["-p", "summarise", "@notes.txt", "again"],
        "piped text",
        false,
        false,
    );
    assert_eq!(result.code, 0, "stderr: {}", result.err);
    assert_eq!(result.out, "second\n", "the last prompt's reply is printed");
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "one request per message");
    let first = format!("{:?}", requests[0].messages);
    assert!(first.contains("piped text"), "stdin first: {first}");
    assert!(first.contains("file body"), "file text: {first}");
    assert!(first.contains("summarise"), "message: {first}");
}

/// `--continue`, `--session <path>` and `--no-session` open the sessions they name.
#[test]
fn continue_resume_session_flags() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("one"),
            ScriptTurn::text("two"),
            ScriptTurn::text("three"),
            ScriptTurn::text("four"),
        ],
        FakeOpts::default(),
    )
    .expect("fake");
    let envv = env_of(&env, &server.url());
    let store = FsStore::new(env.agent_dir().join("sessions"));
    let sessions = || store.list(None);

    let first = go_with(&envv, &env.work(), &["-p", "first"], "", false, false);
    assert_eq!(first.code, 0, "stderr: {}", first.err);
    assert_eq!(sessions().len(), 1, "a new session file");

    let cont = go_with(&envv, &env.work(), &["-c", "-p", "second"], "", false, false);
    assert_eq!(cont.code, 0, "stderr: {}", cont.err);
    assert_eq!(sessions().len(), 1, "--continue appends to the same file");
    let second_request = format!("{:?}", server.requests()[1].messages);
    assert!(
        second_request.contains("first"),
        "continued from the earlier turn: {second_request}"
    );

    let path = sessions()[0].sref.path.display().to_string();
    let resumed = go_with(
        &envv,
        &env.work(),
        &["--session", &path, "-p", "third"],
        "",
        false,
        false,
    );
    assert_eq!(resumed.code, 0, "stderr: {}", resumed.err);
    let third_request = format!("{:?}", server.requests()[2].messages);
    assert!(
        third_request.contains("second"),
        "resumed the same branch: {third_request}"
    );

    let ephemeral = go_with(
        &envv,
        &env.work(),
        &["--no-session", "-p", "fourth"],
        "",
        false,
        false,
    );
    assert_eq!(ephemeral.code, 0, "stderr: {}", ephemeral.err);
    assert_eq!(sessions().len(), 1, "--no-session writes nothing");

    let conflict = go_with(&envv, &env.work(), &["-c", "-r", "-p", "x"], "", false, false);
    assert_eq!(conflict.code, 2, "--continue with --resume is a usage error");
}

/// bwrap missing
#[test]
fn no_sandbox_notice() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![ScriptTurn::text("still runs")], FakeOpts::default()).expect("fake");
    let mut pairs = env.envs(&server.url());
    for (k, v) in pairs.iter_mut() {
        if k == "PATH" {
            // An empty PATH holds no bwrap, so the sandbox is missing.
            *v = String::new();
        }
    }
    let owned = pairs;
    let borrowed: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let envv = Env::from_pairs(&borrowed);
    let result = go_with(&envv, &env.work(), &["-p", "hi"], "", false, false);
    assert_eq!(
        result.code, 0,
        "a missing sandbox does not stop the run: {}",
        result.err
    );
    assert_eq!(result.out, "still runs\n");
    assert!(
        result.err.contains("bwrap is missing or unusable"),
        "notice: {}",
        result.err
    );
    assert!(result.err.contains("bash is disabled"), "notice: {}", result.err);
}

/// H: keybindings.json diagnostics reach stderr in print mode (they were dropped before).
#[test]
fn keymap_diagnostics_reported_in_print_mode() {
    let env = TestEnv::new();
    env.write("home/.oxa/agent/keybindings.json", "{ not json");
    let server = FakeServer::start(vec![ScriptTurn::text("ok")], FakeOpts::default()).expect("fake");
    let c = go(&env, &server.url(), &["-p", "--no-session", "hi"]);
    assert_eq!(c.code, 0, "{}", c.err);
    assert!(
        c.err
            .lines()
            .any(|l| l.starts_with("Warning:") && l.contains("keybindings.json")),
        "{}",
        c.err
    );
}
