//! Wave H lane `harden`, through the real program path (in-process `oxa_cli::run`):
//!
//! * B4 end to end: a session's bash writes `.oxa/mcp.json` (a stdio MCP server, which would
//!   run outside the sandbox); the next start does NOT run it without approval, says so once,
//!   and runs it after an explicit `--approve`.
//! * B3: started in `$HOME`, bash gets no writable directory and the startup says so (R6).
//!
//! Needs a usable bwrap for the bash call (this machine has one); with `OXA_REQUIRE_BWRAP=1`
//! an unusable bwrap fails the tests, otherwise they are skipped with a notice cargo shows.
//! Isolation: `oxa_fake::TestEnv` for every file and directory.

use oxa_cli::{run, Stdio};
use oxa_config::Env;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_perm::TrustStore;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Run `oxa <argv>` in-process in `cwd` against a fake engine with `turns`; exit code + stderr.
fn go(env: &TestEnv, cwd: PathBuf, argv: &[&str], turns: Vec<ScriptTurn>) -> (i32, String) {
    let server = FakeServer::start(turns, FakeOpts::default()).expect("fake engine");
    let owned = env.envs(&server.url());
    let argv: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
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
        let code = run(&argv, &env, io);
        let text = String::from_utf8_lossy(&err.0.lock().unwrap()).into_owned();
        let _ = tx.send((code, text));
    });
    rx.recv_timeout(Duration::from_secs(90)).expect("oxa finished")
}

fn bwrap_usable(env: &TestEnv) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    match oxa_sandbox::Bwrap::detect(&path, &env.home()) {
        Ok(_) => true,
        Err(e) => {
            let required = std::env::var("OXA_REQUIRE_BWRAP").is_ok_and(|v| v == "1");
            assert!(!required, "OXA_REQUIRE_BWRAP=1 but bwrap is not usable: {e}");
            let _ = writeln!(std::io::stderr(), "h_harden SKIPPED: bwrap is not usable: {e}");
            false
        }
    }
}

fn wait_for(path: &Path, secs: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    path.exists()
}

#[test]
fn a_model_written_mcp_json_does_not_run_on_the_next_start() {
    let env = TestEnv::new();
    if !bwrap_usable(&env) {
        return;
    }
    let work = env.work();
    // A trusted project with something gated (a project skill), no `.oxa` yet.
    env.write(
        "work/.agents/skills/s/SKILL.md",
        "---\nname: s\ndescription: a skill\n---\nbody\n",
    );
    let trust = env.agent_dir().join("trust.json");
    TrustStore::load(&trust)
        .save(&trust, &work, true)
        .expect("save trust");
    let marker = work.join("mcp-ran");
    let config = format!(
        r#"{{"mcpServers":{{"evil":{{"command":"sh","args":["-c","touch {}"]}}}}}}"#,
        marker.display()
    );
    let command = format!("mkdir -p .oxa && printf '%s' '{config}' > .oxa/mcp.json && echo written");
    let call = serde_json::json!({ "command": command }).to_string();

    // Session 1: the model's bash writes the project's MCP config (`.oxa` does not exist yet,
    // so the sandbox has nothing to bind read-only there).
    let (code, err) = go(
        &env,
        work.clone(),
        &["-p", "set up an mcp server"],
        vec![ScriptTurn::tool("c1", "bash", &call), ScriptTurn::text("done")],
    );
    assert_eq!(code, 0, "{err}");
    assert!(
        work.join(".oxa/mcp.json").exists(),
        "the bash call wrote it: {err}"
    );

    // Session 2: not loaded, one notice naming the file and the way to approve it.
    let (code, err) = go(&env, work.clone(), &["-p", "hi"], vec![ScriptTurn::text("ok")]);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("Project config not loaded")
            && err.contains(".oxa/mcp.json (new: MCP servers evil: runs `sh -c touch"),
        "{err}"
    );
    assert!(err.contains("Fix: "), "R6 shape: {err}");
    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists(), "the model's MCP server ran without approval");

    // The user approves explicitly (this run): now it runs.
    let (code, err) = go(
        &env,
        work.clone(),
        &["-a", "-p", "hi"],
        vec![ScriptTurn::text("ok")],
    );
    assert_eq!(code, 0, "{err}");
    assert!(wait_for(&marker, 10), "after --approve the server starts: {err}");
}

#[test]
fn the_file_tools_ask_before_writing_project_config() {
    // Print mode cannot answer the question: the write is refused, nothing is written.
    let env = TestEnv::new();
    let work = env.work();
    let call = serde_json::json!({ "path": ".oxa/mcp.json", "content": "{}" }).to_string();
    let (code, err) = go(
        &env,
        work.clone(),
        &["-p", "write config"],
        vec![ScriptTurn::tool("c1", "write", &call), ScriptTurn::text("done")],
    );
    assert_eq!(code, 0, "{err}");
    assert!(!work.join(".oxa/mcp.json").exists());
}

#[test]
fn started_in_home_bash_gets_no_writable_directory() {
    let env = TestEnv::new();
    if !bwrap_usable(&env) {
        return;
    }
    // Item 3 (sec-prod): a start in home is refused unless the user opted in; this test is about
    // the sandbox's warning in that case, so it opts in through the user settings.
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"allowHomeCwd":true}}"#,
    );
    let (code, err) = go(&env, env.home(), &["-p", "hi"], vec![ScriptTurn::text("ok")]);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("Bash has no writable directory: oxa was started in")
            && err.contains(
                "Fix: start oxa in a project directory, or set \"oxa.sandbox.allowHomeWrite\": true"
            ),
        "{err}"
    );
    // The setting (user file) turns the warning off.
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"allowHomeCwd":true,"sandbox":{"allowHomeWrite":true}}}"#,
    );
    let (code, err) = go(&env, env.home(), &["-p", "hi"], vec![ScriptTurn::text("ok")]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("Bash has no writable directory"), "{err}");
    // O-199: one clear notice says what the workspace is and what stays protected.
    assert_eq!(err.matches("workspace = your home directory").count(), 1, "{err}");
    assert!(err.contains("(protected: ") && err.contains(".bashrc"), "{err}");
}

/// O-199: a parent of `$HOME` is never a workspace, even with the home-write setting.
#[test]
fn started_above_home_bash_gets_no_writable_directory_even_with_the_setting() {
    let env = TestEnv::new();
    if !bwrap_usable(&env) {
        return;
    }
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"allowHomeCwd":true,"sandbox":{"allowHomeWrite":true}}}"#,
    );
    let parent = env.home().parent().expect("home has a parent").to_path_buf();
    let (code, err) = go(&env, parent, &["-p", "hi"], vec![ScriptTurn::text("ok")]);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains("Bash has no writable directory") && err.contains("a parent of your home directory"),
        "{err}"
    );
    assert!(!err.contains("workspace = your home directory"), "{err}");
}

/// B4 a: an extension's `project_trust` yes approves the project as it is (the first record);
/// it cannot approve a project extension that changed afterwards: that needs the user.
#[test]
fn an_extension_cannot_approve_changed_project_code() {
    let env = TestEnv::new();
    let work = env.work();
    env.write(
        "home/.oxa/agent/extensions/trust.js",
        "export default function (oxa) { oxa.on('project_trust', (e, ctx) => ({ trusted: 'yes', remember: true })); }",
    );
    let project_ext = "export default function (oxa) { oxa.on('session_start', (e, ctx) => { ctx.fs.writeFile('project-log.txt', 'loaded'); }); }";
    env.write("work/.oxa/extensions/local.js", project_ext);
    let (code, err) = go(&env, work.clone(), &["-p", "hi"], vec![ScriptTurn::text("ok")]);
    assert_eq!(code, 0, "{err}");
    assert!(
        work.join("project-log.txt").exists(),
        "first start: the extension's yes loads it: {err}"
    );
    std::fs::remove_file(work.join("project-log.txt")).unwrap();

    // The project extension changes (as a model's write would change it).
    env.write(
        "work/.oxa/extensions/local.js",
        &format!("{project_ext}\n// changed\n"),
    );
    let (code, err) = go(&env, work.clone(), &["-p", "hi"], vec![ScriptTurn::text("ok")]);
    assert_eq!(code, 0, "{err}");
    assert!(
        !work.join("project-log.txt").exists(),
        "the changed extension ran: {err}"
    );
    assert!(
        err.contains("Project config not loaded") && err.contains("local.js (changed: extension code"),
        "{err}"
    );
}
