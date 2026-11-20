//! Exercise goal recovery through the real terminal loop, with only synthetic model replies.
use oxa_fake::{FakeModel, FakeOpts, FakeServer, Fault, ScriptTurn, TestEnv};
use oxa_session::{FsStore, SessionStore};
use std::process::Command;

const PTY: &str = r#"
import fcntl, os, pty, re, select, signal, struct, subprocess, sys, termios, time
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 32, 160, 0, 0))
child = subprocess.Popen([sys.argv[1], '--yolo', '--model', 'fake-model',
    '--no-mcp', '--no-extensions', '--no-skills', '--no-context-files', '--tui-mode', 'regular'],
    stdin=slave, stdout=slave, stderr=slave, close_fds=True)
os.close(slave)
output = b''
cursor = 0
def wait_for(marker):
    global output, cursor
    deadline = time.monotonic() + 10
    while output.find(marker, cursor) < 0:
        if child.poll() is not None or time.monotonic() >= deadline:
            text = re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', output).decode(errors='replace')
            lines = dict.fromkeys(line.strip() for line in text.splitlines() if line.strip())
            raise AssertionError(f'missing {marker!r}:\n' + '\n'.join(lines)[-12000:])
        if select.select([master], [], [], .1)[0]:
            output += os.read(master, 65536)
    cursor = output.index(marker, cursor) + len(marker)
try:
    wait_for(b'READY')
    os.write(master, b'/goal Finish the synthetic task\r')
    case = sys.argv[2]
    if case == 'quota':
        wait_for(b'Provider quota exhausted')
        wait_for(b'/goal resume')
    else:
        wait_for(b'Goal waiting 60 s')
        if case == 'switch':
            os.write(master, b'\x0c')
            wait_for(b'Select model')
            os.write(master, b'replacement')
            wait_for(b'search: replacement')
            wait_for(b'replacement-model')
            os.write(master, b'\r')
        else:
            os.write(master, b'Use the synthetic instruction\r')
        wait_for(b'Synthetic proof')
    child.send_signal(signal.SIGHUP)
    child.wait(timeout=5)
finally:
    os.close(master)
    if child.poll() is None:
        child.kill()
    child.wait(timeout=5)
"#;

fn exercise(case: &str) {
    let env = TestEnv::new();
    env.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({
            "oxa": {"engine": {"inProcessCloud": false, "inProcessLocal": false}},
            "cacheWarming": "off",
            "retry": {"enabled": case == "quota", "provider": {"maxRetries": 1, "maxRetryDelayMs": 0}}
        })
        .to_string(),
    );
    let fault = Fault::RateLimited {
        retry_after: if case == "quota" { 205089 } else { 900 },
    };
    let mut finish = ScriptTurn::tool(
        "finish",
        "goal_done",
        r#"{"status":"complete","evidence":"Synthetic proof"}"#,
    );
    finish.text = Some("Synthetic proof".into());
    let server = FakeServer::start(
        vec![ScriptTurn::fault(fault), finish],
        FakeOpts {
            models: ["fake-model", "replacement-model"]
                .into_iter()
                .map(|alias| FakeModel {
                    alias: alias.into(),
                    cap: 32768,
                    max_output: None,
                    caps: None,
                })
                .collect(),
            ..FakeOpts::default()
        },
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
    let requests: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|r| r.max_tokens != 0)
        .collect();
    assert!(
        output.status.success(),
        "requested models: {:?}\n{}",
        requests.iter().map(|r| &r.model).collect::<Vec<_>>(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(requests[0].model, "fake-model");
    if case == "quota" {
        assert_eq!(requests.len(), 1, "quota stop must not send another request");
        return;
    }
    assert_eq!(requests.len(), 2);
    assert_ne!(requests[0].idem_key, requests[1].idem_key);
    if case == "switch" {
        assert_eq!(requests[1].model, "replacement-model");
        let store = FsStore::new(env.agent_dir().join("sessions"));
        let sessions = store.list(None);
        assert_eq!(sessions.len(), 1);
        assert!(std::fs::read_to_string(&sessions[0].sref.path)
            .unwrap()
            .contains("replacement-model"));
    } else {
        assert_eq!(requests[1].model, "fake-model");
        assert!(requests[1]
            .messages
            .iter()
            .any(|m| m.content.as_deref() == Some("Use the synthetic instruction")));
    }
}

#[test]
fn model_picker_retries_the_goal_immediately_on_the_selected_model() {
    exercise("switch");
}

#[test]
fn typing_during_backoff_updates_the_retry_prompt() {
    exercise("input");
}

#[test]
fn long_retry_after_stops_with_recovery_instructions() {
    exercise("quota");
}
