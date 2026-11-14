//! Extension session API end to end: `ctx.newSession()` / `ctx.switchSession(path)` from an
//! extension are refused with `unsupported_in_oxa` (O-175; the user's /new and /resume are the
//! way), and an extension tool's `details` survive a resume. Runs `oxa_cli::run` in-process
//! against `oxa-fake`; the files live under `oxa_fake::TestEnv`.

use oxa_cli::{run, Stdio};
use oxa_config::Env;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
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

impl Sink {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

const EXTENSION: &str = r#"
export default function (oxa) {
  oxa.registerCommand("fresh", { handler: (args, ctx) => { ctx.newSession(); } });
  oxa.registerCommand("goto", { handler: (args, ctx) => { ctx.switchSession(args); } });
}
"#;

/// Run the program with `stdin` and `words`; returns (code, stdout, stderr).
fn go(env: &TestEnv, url: &str, words: &[&str], stdin: &str) -> (i32, String, String) {
    let owned = env.envs(url);
    let cwd = env.work();
    let argv: Vec<String> = words.iter().map(|s| s.to_string()).collect();
    let input = stdin.as_bytes().to_vec();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let env = Env::from_pairs(&pairs);
        let (out, err) = (Sink::default(), Sink::default());
        let io = Stdio {
            stdin: Box::new(Cursor::new(input)),
            stdout: Box::new(out.clone()),
            stderr: Box::new(err.clone()),
            stdin_is_tty: false,
            stdout_is_tty: false,
            cwd,
        };
        let code = run(&argv, &env, io);
        let _ = tx.send((code, out.text(), err.text()));
    });
    rx.recv_timeout(Duration::from_secs(60))
        .expect("oxa finished in time")
}

fn session_files(env: &TestEnv) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for p in rd.flatten().map(|e| e.path()) {
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&env.agent_dir().join("sessions"), &mut out);
    out.sort();
    out
}

fn setup() -> TestEnv {
    let env = TestEnv::new();
    let path = env.agent_dir().join("extensions").join("swap.js");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, EXTENSION).unwrap();
    env
}

fn containing(env: &TestEnv, needle: &str) -> Vec<PathBuf> {
    session_files(env)
        .into_iter()
        .filter(|p| std::fs::read_to_string(p).unwrap_or_default().contains(needle))
        .collect()
}

/// O-175: the requests are refused; the run goes on in the same session file.
#[test]
fn new_and_switch_session_from_an_extension_are_unsupported() {
    let env = setup();
    let fake = FakeServer::start(
        vec![ScriptTurn::text("one"), ScriptTurn::text("two")],
        FakeOpts::default(),
    )
    .expect("fake");
    let (_code, out, err) = go(
        &env,
        &fake.url(),
        &[
            "-p",
            "BEFORE-FRESH",
            "/fresh",
            "/goto elsewhere.jsonl",
            "AFTER-GOTO",
        ],
        "",
    );
    let both = format!("{out}{err}");
    assert_eq!(
        both.matches("newSession and switchSession from an extension are not supported in oxa")
            .count(),
        2,
        "both requests are refused: {both}"
    );
    assert_eq!(
        containing(&env, "BEFORE-FRESH"),
        containing(&env, "AFTER-GOTO"),
        "one session file: {:?}",
        session_files(&env)
    );
    assert_eq!(session_files(&env).len(), 1);
}

/// Item 8 of the lane: an extension tool's result `details` survive a resume (pi keeps them on
/// the toolResult message; the todo example rebuilds its state from them at session_start).
#[test]
fn tool_details_survive_a_resume() {
    let env = TestEnv::new();
    let path = env.agent_dir().join("extensions").join("keep.js");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        r#"export default function (oxa) {
  oxa.registerTool({ name: "keep", description: "keeps", class: "read",
    parameters: { type: "object", properties: {} },
    execute: async () => ({ content: [{ type: "text", text: "kept" }], details: { n: 7 } }) });
  // (A command, not session_start: a resume does not fire session_start yet; hooks lane.)
  oxa.registerCommand("check", { handler: (args, ctx) => {
    for (const entry of ctx.sessionManager.getBranch()) {
      const m = entry.message;
      if (entry.type === "message" && m.role === "toolResult" && m.toolName === "keep") {
        ctx.fs.writeFile("details.txt", "details=" + m.details.n);
      }
    }
  } });
}
"#,
    )
    .unwrap();
    let fake = FakeServer::start(
        vec![ScriptTurn::tool("c1", "keep", "{}"), ScriptTurn::text("done")],
        FakeOpts::default(),
    )
    .expect("fake");
    let (code, _o, err) = go(&env, &fake.url(), &["-p", "keep it"], "");
    assert_eq!(code, 0, "{err}");
    let fake = FakeServer::start(vec![ScriptTurn::text("again")], FakeOpts::default()).expect("fake");
    let (code, _o, err) = go(&env, &fake.url(), &["-c", "-p", "/check"], "");
    assert_eq!(code, 0, "{err}");
    let text = std::fs::read_to_string(env.work().join("details.txt")).unwrap_or_default();
    assert_eq!(text, "details=7", "{err}");
}
