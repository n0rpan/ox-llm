//! The invocation's limits and SIGTERM in the real interactive process: they follow a session
//! swap (`/new`), the first SIGTERM is cooperative and the second one ends the process.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::process::Command;

const PTY: &str = r#"
import os, pty, select, signal, subprocess, sys, time
master, slave = pty.openpty()
case = sys.argv[2]
limit = ['--max-tokens', '1'] if case == 'new' else ['--max-time', '600']
child = subprocess.Popen([sys.argv[1], '--yolo', '--model', 'fake-model', '--no-mcp',
    '--no-extensions', '--no-skills', '--no-context-files'] + limit,
    stdin=slave, stdout=slave, stderr=slave, close_fds=True)
os.close(slave)
output = b''
def pump(seconds=.1):
    global output
    if select.select([master], [], [], seconds)[0]:
        try:
            output += os.read(master, 65536)
        except OSError:
            pass
def wait_for(ready, what):
    deadline = time.monotonic() + 20
    while not ready():
        assert child.poll() is None, 'exited before ' + what
        assert time.monotonic() < deadline, 'deadline for ' + what
        pump()
def exit_code():
    deadline = time.monotonic() + 30
    while child.poll() is None:
        assert time.monotonic() < deadline, 'process did not exit'
        pump()
    return child.returncode
try:
    wait_for(lambda: b'READY' in output, 'READY')
    if case == 'new':
        os.write(master, b'/new\r')
        wait_for(lambda: b'New session started' in output, 'the new session')
        os.write(master, b'hello\r')
        assert exit_code() == 130, 'exit code ' + str(child.returncode)
    else:
        os.write(master, b'go\r')
        wait_for(lambda: os.path.exists('tool-started'), 'the tool')
        child.send_signal(signal.SIGTERM)
        if case == 'sigterm':
            assert exit_code() == 130, 'exit code ' + str(child.returncode)
        else:
            time.sleep(.5)
            assert child.poll() is None, 'the first SIGTERM ended the process'
            child.send_signal(signal.SIGTERM)
            assert exit_code() == -signal.SIGTERM, 'exit code ' + str(child.returncode)
finally:
    os.close(master)
    if child.poll() is None:
        child.kill()
    child.wait(timeout=5)
"#;

fn run(case: &str, turns: Vec<ScriptTurn>) -> (TestEnv, FakeServer) {
    let env = TestEnv::new();
    let server = FakeServer::start(turns, FakeOpts::default()).unwrap();
    std::fs::write(
        env.home().join(".oxa/agent/settings.json"),
        r#"{"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false}},"cacheWarming":"off","permissions":{"defaultMode":"yolo"},"retry":{"enabled":false}}"#,
    )
    .unwrap();
    let output = Command::new("python3")
        .args(["-I", "-c", PTY, env!("CARGO_BIN_EXE_oxa"), case])
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("TERM", "xterm-256color")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (env, server)
}

fn last_user(request: &oxa_fake::Recorded) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|m| m.role == oxa_proto::Role::User)
        .and_then(|m| m.content.clone())
        .unwrap_or_default()
}

fn held_tool(seconds: u32) -> ScriptTurn {
    ScriptTurn::tool(
        "hold",
        "bash",
        &format!(
            r#"{{"command":"printf started > tool-started; sleep {seconds}; printf finished > tool-finished"}}"#
        ),
    )
}

#[test]
fn a_token_limit_still_applies_after_new() {
    let (_env, server) = run(
        "new",
        vec![
            ScriptTurn::text("first reply"),
            ScriptTurn::text("final summary"),
            ScriptTurn::text("unexpected later reply"),
        ],
    );
    let chats: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.max_tokens > 0)
        .collect();
    assert_eq!(chats.len(), 2, "one turn, then one wrap-up turn");
    assert!(last_user(&chats[1]).contains("Wrap up now"));
}

#[test]
fn the_first_sigterm_is_cooperative_in_the_interactive_run() {
    let (env, server) = run("sigterm", vec![held_tool(2), ScriptTurn::text("wrapped up")]);
    assert!(
        env.work().join("tool-finished").exists(),
        "the held tool was allowed to finish"
    );
    let chats: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.max_tokens > 0)
        .collect();
    assert_eq!(chats.len(), 2);
    assert!(last_user(&chats[1]).contains("Wrap up now"));
}

#[test]
fn the_second_sigterm_ends_the_interactive_process() {
    let (env, server) = run("sigterm2", vec![held_tool(20), ScriptTurn::text("unused")]);
    assert!(!env.work().join("tool-finished").exists());
    assert_eq!(
        server.requests().iter().filter(|r| r.max_tokens > 0).count(),
        1,
        "no wrap-up turn after the second SIGTERM"
    );
}
