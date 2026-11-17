//! Item 3 (sec-prod): a session started in `$HOME`, `/` or a parent of `$HOME` is refused in
//! print mode (interactive asks, default No), unless the user opted in (`oxa.allowHomeCwd` in
//! the user settings, or `OXA_ALLOW_HOME_CWD`). A project below home is fine. Refusals happen
//! before the engine is contacted, so the refusal tests need no server.
use oxa_cli::{run, Stdio};
use oxa_config::Env;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::io::{Cursor, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

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

fn text(s: &Sink) -> String {
    String::from_utf8(s.0.lock().unwrap().clone()).unwrap()
}

/// One `oxa` run with `cwd`, `extra_env`, `stdin` text and the tty flags.
fn go(
    env: &TestEnv,
    engine_url: &str,
    cwd: &Path,
    extra_env: &[(&str, &str)],
    words: &[&str],
    stdin: &str,
    tty: bool,
) -> (i32, String, String) {
    let mut owned = env.envs(engine_url);
    owned.extend(extra_env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let (out, err) = (Sink::default(), Sink::default());
    let io = Stdio {
        stdin: Box::new(Cursor::new(stdin.as_bytes().to_vec())),
        stdout: Box::new(out.clone()),
        stderr: Box::new(err.clone()),
        stdin_is_tty: tty,
        stdout_is_tty: tty,
        cwd: cwd.to_path_buf(),
    };
    let argv: Vec<String> = words.iter().map(|s| s.to_string()).collect();
    let code = run(&argv, &Env::from_pairs(&pairs), io);
    (code, text(&out), text(&err))
}

const REFUSED: &str = "oxa was started in";
const UNREACHABLE: &str = "http://127.0.0.1:9";

fn refused_in(env: &TestEnv, cwd: &Path) {
    let (code, out, err) = go(
        env,
        UNREACHABLE,
        cwd,
        &[],
        &["-p", "--no-session", "x"],
        "",
        false,
    );
    assert_ne!(code, 0, "{cwd:?}: {err}");
    assert!(err.contains(REFUSED), "{err}");
    assert!(err.contains("cd into a project directory first"), "{err}");
    assert!(out.is_empty(), "nothing on stdout: {out}");
}

#[test]
fn print_mode_refuses_at_home() {
    let env = TestEnv::new();
    refused_in(&env, &env.home());
}

#[test]
fn print_mode_refuses_at_root_and_at_a_parent_of_home() {
    let env = TestEnv::new();
    refused_in(&env, Path::new("/"));
    refused_in(&env, env.root());
}

#[test]
fn json_and_rpc_refuse_too() {
    let env = TestEnv::new();
    let home = env.home();
    let (code, _, err) = go(
        &env,
        UNREACHABLE,
        &home,
        &[],
        &["--mode", "json", "--no-session", "x"],
        "",
        false,
    );
    assert_ne!(code, 0);
    assert!(err.contains(REFUSED), "{err}");
    let (code, _, err) = go(
        &env,
        UNREACHABLE,
        &home,
        &[],
        &["--mode", "rpc", "--no-session"],
        "",
        false,
    );
    assert_ne!(code, 0);
    assert!(err.contains(REFUSED), "{err}");
}

#[test]
fn a_project_below_home_is_fine() {
    let env = TestEnv::new();
    let project = env.home().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let server = FakeServer::start(vec![ScriptTurn::text("ok")], FakeOpts::default()).unwrap();
    let (code, out, err) = go(
        &env,
        &server.url(),
        &project,
        &[],
        &["-p", "--no-session", "x"],
        "",
        false,
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out.trim(), "ok");
}

#[test]
fn the_user_setting_opts_in() {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"allowHomeCwd":true}}"#,
    );
    let server = FakeServer::start(vec![ScriptTurn::text("ok")], FakeOpts::default()).unwrap();
    let home = env.home();
    let (code, out, err) = go(
        &env,
        &server.url(),
        &home,
        &[],
        &["-p", "--no-session", "x"],
        "",
        false,
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out.trim(), "ok");
}

#[test]
fn a_project_setting_cannot_opt_in() {
    let env = TestEnv::new();
    env.write("home/.oxa/settings.json", r#"{"oxa":{"allowHomeCwd":true}}"#);
    let home = env.home();
    refused_in(&env, &home);
}

#[test]
fn the_environment_opts_in() {
    let env = TestEnv::new();
    let home = env.home();
    // Every shared true word opts in, in any case.
    let true_words = ["1", "true", "TRUE", "yes", "on", "ON"];
    let server = FakeServer::start(
        true_words.iter().map(|_| ScriptTurn::text("ok")).collect(),
        FakeOpts::default(),
    )
    .unwrap();
    for value in true_words {
        let (code, out, err) = go(
            &env,
            &server.url(),
            &home,
            &[("OXA_ALLOW_HOME_CWD", value)],
            &["-p", "--no-session", "x"],
            "",
            false,
        );
        assert_eq!(code, 0, "OXA_ALLOW_HOME_CWD={value}: {err}");
        assert_eq!(out.trim(), "ok");
    }
    // A falsy, unknown or empty value does not opt in.
    for value in ["0", "false", "no", "off", "", "maybe"] {
        refused_in_with_env(&env, &home, value);
    }
}

fn refused_in_with_env(env: &TestEnv, cwd: &Path, value: &str) {
    let (code, _, err) = go(
        env,
        UNREACHABLE,
        cwd,
        &[("OXA_ALLOW_HOME_CWD", value)],
        &["-p", "--no-session", "x"],
        "",
        false,
    );
    assert_ne!(code, 0, "OXA_ALLOW_HOME_CWD={value}: {err}");
    assert!(err.contains(REFUSED), "{err}");
}

#[test]
fn interactive_mode_asks_and_the_default_is_no() {
    let env = TestEnv::new();
    let home = env.home();
    // An empty answer is the default No: one line, non-zero exit, the warning on stdout.
    let (code, out, err) = go(&env, UNREACHABLE, &home, &[], &[], "\n", true);
    assert_ne!(code, 0);
    assert!(out.contains("Start oxa here anyway? [y/N]"), "{out}");
    assert!(err.contains("cd into a project directory first"), "{err}");
    // An explicit "n" declines the same way.
    let (code, _, _) = go(&env, UNREACHABLE, &home, &[], &[], "n\n", true);
    assert_ne!(code, 0);
}

#[test]
fn subcommands_and_version_are_not_affected() {
    let env = TestEnv::new();
    let home = env.home();
    let (code, out, _) = go(&env, UNREACHABLE, &home, &[], &["--version"], "", false);
    assert_eq!(code, 0);
    assert!(out.starts_with("oxa "), "{out}");
}
