//! Wave H (lane hooks): `project_trust` at startup, through the real program path.
//!
//! pi (`core/project-trust.ts` `resolveProjectTrusted`): when the project has trust-gated
//! resources and no flag decides, the user-level and `-e` extensions get `project_trust` with
//! `{cwd}`; the first `yes` / `no` wins over the saved decision and the default, `remember:
//! true` saves it, `undecided` falls through. Project extensions are not loaded before.
//! Isolation: `oxa_fake::TestEnv` for every file and directory.

#[path = "common/http.rs"]
mod http;
use oxa_cli::{run, Stdio};
use oxa_config::Env;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// Run `oxa -p hi` in-process; returns the exit code and stderr.
fn go(env: &TestEnv) -> (i32, String) {
    let server = FakeServer::start(vec![ScriptTurn::text("ok")], FakeOpts::default()).expect("fake engine");
    let owned = http::envs(env, &server.url());
    let cwd = env.work();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let env = Env::from_pairs(&pairs);
        let err = Sink::default();
        let io = Stdio {
            stdin: Box::new(Cursor::new(Vec::new())),
            stdout: Box::new(Sink::default()),
            stderr: Box::new(err.clone()),
            stdin_is_tty: false,
            stdout_is_tty: false,
            cwd,
        };
        let argv = vec!["-p".to_string(), "hi".to_string()];
        let code = run(&argv, &env, io);
        let text = String::from_utf8_lossy(&err.0.lock().unwrap()).into_owned();
        let _ = tx.send((code, text));
    });
    rx.recv_timeout(Duration::from_secs(60)).expect("oxa finished")
}

/// A user-level extension answering `project_trust` with `answer` (a JS object literal); it
/// logs the event it got to `trust-log.txt` in the work directory.
fn user_ext(env: &TestEnv, answer: &str) {
    env.write(
        "home/.oxa/agent/extensions/trust.js",
        &format!(
            "export default function (oxa) {{ oxa.on('project_trust', (e, ctx) => {{ ctx.fs.writeFile('trust-log.txt', e.type + ' ' + e.cwd); return {answer}; }}); }}"
        ),
    );
}

/// A project extension: logs `loaded` when it runs (only when the project is trusted).
fn project_ext(env: &TestEnv) {
    env.write(
        "work/.oxa/extensions/local.js",
        "export default function (oxa) { oxa.on('session_start', (e, ctx) => { ctx.fs.writeFile('project-log.txt', 'loaded'); }); }",
    );
}

#[test]
fn extension_decides_trust() {
    let env = TestEnv::new();
    project_ext(&env);
    user_ext(&env, "{ trusted: 'yes', remember: true }");
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    let log = std::fs::read_to_string(env.work().join("trust-log.txt")).expect("project_trust fired");
    assert!(log.starts_with("project_trust "), "{log}");
    assert!(
        log.ends_with(&env.work().display().to_string()) || log.contains("work"),
        "{log}"
    );
    assert!(
        env.work().join("project-log.txt").exists(),
        "the extension's yes trusts the project: its extension loads"
    );
    let saved = std::fs::read_to_string(env.agent_dir().join("trust.json")).expect("remember saves");
    assert!(saved.contains("true"), "{saved}");
}

#[test]
fn undecided_falls_through_to_the_default() {
    let env = TestEnv::new();
    project_ext(&env);
    user_ext(&env, "{ trusted: 'undecided' }");
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    assert!(env.work().join("trust-log.txt").exists(), "project_trust fired");
    assert!(
        !env.work().join("project-log.txt").exists(),
        "no decision, not interactive: the project stays untrusted"
    );
    assert!(!env.agent_dir().join("trust.json").exists(), "nothing saved");
}

#[test]
fn not_fired_without_gated_resources() {
    let env = TestEnv::new();
    user_ext(&env, "{ trusted: 'no' }");
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    assert!(
        !env.work().join("trust-log.txt").exists(),
        "nothing to trust: no event"
    );
}

/// #201 (3): a project trusted by a saved decision, with no project config when a session
/// starts, gets its (empty) approval recorded at that start. `.oxa` content the session's tools
/// then create is new at the next start and needs the normal approval instead of being taken
/// as the project's first record.
#[test]
fn config_created_during_a_session_is_not_auto_approved_next_start() {
    let env = TestEnv::new();
    let trust = env.agent_dir().join("trust.json");
    let mut store = oxa_perm::TrustStore::load(&trust);
    store.save(&trust, &env.work(), true).expect("save trust");
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    // What a tool of that session wrote (the sandbox leaves `.oxa` writable on purpose).
    env.write("work/.oxa/settings.json", r#"{"done": ["echo planted"]}"#);
    let (code, err) = go(&env);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("Project config not loaded"), "{err}");
    assert!(err.contains("settings.json"), "{err}");
    let store = oxa_perm::TrustStore::load(&trust);
    assert!(
        matches!(store.approval(&env.work()), oxa_perm::Approval::Changed(_)),
        "the planted file is still not approved"
    );
}
