//! #861: a lost PTY must stop the real interactive process, not leave a spinning orphan.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::process::Command;

const PTY_CLOSE: &str = r#"
import os, pty, select, signal, subprocess, sys, time
master, slave = pty.openpty()
child = subprocess.Popen([sys.argv[1], '--model', 'fake-model', '--no-mcp',
    '--no-extensions', '--no-skills', '--no-context-files'],
    stdin=slave, stdout=slave, stderr=slave, close_fds=True)
os.close(slave)
try:
    output = b''
    deadline = time.monotonic() + 15
    while b'READY' not in output:
        assert child.poll() is None, 'interactive process exited before READY'
        assert time.monotonic() < deadline, 'interactive startup deadline'
        if select.select([master], [], [], .1)[0]:
            output += os.read(master, 65536)
    # No controlling terminal: closing the master isolates EOF/EIO from automatic SIGHUP.
    os.close(master)
    master = None
    try:
        code = child.wait(timeout=3)
    except subprocess.TimeoutExpired:
        raise AssertionError('terminal disappeared but interactive oxa stayed alive')
    assert code == 129, 'terminal-loss exit: ' + str(code)
finally:
    if master is not None:
        os.close(master)
    if child.poll() is None:
        child.kill()
    child.wait(timeout=3)
"#;

#[test]
fn lost_pty_exits_promptly_with_hangup_status() {
    let env = TestEnv::new();
    let server = FakeServer::start(vec![ScriptTurn::text("unused")], FakeOpts::default()).unwrap();
    std::fs::write(
        env.home().join(".oxa/agent/settings.json"),
        r#"{"oxa":{"engine":{"inProcessCloud":false,"inProcessLocal":false}},"cacheWarming":"off","retry":{"enabled":false}}"#,
    )
    .unwrap();
    let output = Command::new("python3")
        .args(["-I", "-c", PTY_CLOSE, env!("CARGO_BIN_EXE_oxa")])
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("TERM", "xterm-256color")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(server.requests().is_empty(), "idle terminal loss starts no model turn");
}
