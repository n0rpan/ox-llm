//! Real CLI controls for free-text chat through the protected owner Session.
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_session::{FsStore, SessionStore};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::{Command, Output};

fn run(test: &TestEnv, server: &FakeServer, words: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(words)
        .env_clear()
        .envs(test.envs(&server.url()))
        .env("OXA_LEAD_BOARD_LOG", test.agent_dir().join("board.jsonl"))
        .current_dir(test.work())
        .output()
        .unwrap()
}

#[test]
fn free_text_refuses_ordinary_sessions_and_does_not_fall_back_to_latest() {
    let test = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("ordinary lane reply"),
            ScriptTurn::text("newer lane reply"),
            ScriptTurn::text(r#"{"text":"unexpected lead reply","facts":[],"action":null}"#),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    for (id, text) in [
        ("ordinary-lane-session", "ordinary lane input"),
        ("newer-lane-session", "newer lane input"),
    ] {
        let output = run(&test, &server, &["-p", "--session-id", id, text]);
        assert!(output.status.success(), "{output:?}");
    }
    let settings = test.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"lead":{"sessionId":"ordinary-lane-session"}}}"#,
    );
    for (path, mode) in [(test.agent_dir(), 0o700), (settings, 0o600)] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let requests_before = server.requests().len();
    let output = run(&test, &server, &["lead", "chat", "Continue the conversation"]);
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(server.requests().len(), requests_before);
}

#[test]
fn dedicated_lead_session_keeps_its_own_history_across_cli_turns() {
    let test = TestEnv::new();
    let dirs = oxa_config::Dirs::under(test.root());
    let uid = std::fs::metadata(test.root()).unwrap().uid();
    std::fs::set_permissions(&dirs.agent_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let project = test.work().canonicalize().unwrap();
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("ordinary reply"),
            ScriptTurn::text(r#"{"text":"first dedicated reply","facts":[2],"action":null}"#),
            ScriptTurn::text(r#"{"text":"second dedicated reply","facts":[],"action":null}"#),
            ScriptTurn::text(r#"{"text":"forged evidence","facts":[99],"action":null}"#),
            ScriptTurn::text(r#"{"text":"unobserved item","facts":[],"action":{"kind":"board","item":45,"change":"park"}}"#),
            ScriptTurn::text(r#"{"text":"invented authority","facts":[],"action":{"kind":"board","item":44,"change":"park","authority":"owner"}}"#),
            ScriptTurn::text(r#"{"text":"hold issue as a PR","facts":[],"action":{"kind":"rule","rule":"hold","scope":{"kind":"pull_request","value":43},"value":{"kind":"constraint","value":{"active":true,"until":null}}}}"#),
            ScriptTurn::text(r#"{"text":"hold for confirmation","facts":[2],"action":{"kind":"rule","rule":"hold","scope":{"kind":"pull_request","value":44},"value":{"kind":"constraint","value":{"active":true,"until":null}}}}"#),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let initialized = run(&test, &server, &["lead", "chat", "--init"]);
    assert!(initialized.status.success(), "{initialized:?}");
    let initialized = String::from_utf8(initialized.stdout).unwrap();
    let id = initialized.trim();
    assert!(!id.is_empty());
    assert!(server.requests().is_empty());
    let ordinary = run(
        &test,
        &server,
        &[
            "-p",
            "--session-id",
            "newer-lane-session",
            "ordinary lane history",
        ],
    );
    assert!(ordinary.status.success(), "{ordinary:?}");
    let settings = test.write(
        "home/.oxa/agent/settings.json",
        &serde_json::json!({"oxa":{"lead":{"sessionId":id}}}).to_string(),
    );
    std::fs::set_permissions(settings, std::fs::Permissions::from_mode(0o600)).unwrap();

    assert!(oxa_cli::LeadOwnerBinding::load(&dirs, &project, uid)
        .unwrap()
        .is_some());

    let entries: Vec<_> = (30..=44)
        .map(|number| {
            serde_json::json!({
                "number": number, "head": "a".repeat(40), "release_pinned": false,
                "action": if number == 43 { "wait_issue" } else { "wait_review" },
                "lease": null, "attempts_today": 0, "eligible": false
            })
        })
        .collect();
    let sample = test.write(
        "home/.oxa/agent/board.jsonl",
        &format!(
            "{}\n",
            serde_json::json!({
                "schema_version": 1, "observed_at": 1234,
                "board": {"observed_at": 1234, "entries": entries}
            })
        ),
    );
    std::fs::set_permissions(sample, std::fs::Permissions::from_mode(0o600)).unwrap();

    let first = "/help is literal conversation text here about #44";
    let second = "!printf is literal conversation text here";
    for (message, response) in [
        (first, "first dedicated reply"),
        (second, "second dedicated reply"),
    ] {
        let output = run(&test, &server, &["lead", "chat", message]);
        assert!(output.status.success(), "{output:?}");
        let displayed = String::from_utf8_lossy(&output.stdout);
        assert!(displayed.contains(response));
        if message == first {
            assert!(displayed.contains("PR #44") && displayed.contains("/items/44"));
        }
    }
    let requests: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|request| request.max_tokens > 0)
        .collect();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert!(request.max_tokens <= 512);
        assert_eq!(request.tools_json, "[]");
        assert!(request.messages.iter().all(|message| {
            !message
                .content
                .as_deref()
                .unwrap_or_default()
                .contains("ordinary lane history")
        }));
    }
    assert!(requests[1].messages.iter().any(|message| {
        message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("Observations:") && text.contains("PR #44"))
    }));
    let last = &requests[2].messages;
    assert!(last
        .iter()
        .any(|message| message.content.as_deref() == Some(first)));
    assert!(last
        .iter()
        .any(|message| message.content.as_deref() == Some(second)));

    let model_requests = || {
        server
            .requests()
            .iter()
            .filter(|request| request.max_tokens > 0)
            .count()
    };
    for message in [
        "Verify the evidence",
        "Consider parking #45",
        "Consider parking #44",
        "Consider a PR hold for #43",
    ] {
        let before = model_requests();
        let output = run(&test, &server, &["lead", "chat", message]);
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert_eq!(model_requests(), before + 1);
    }

    let output = run(&test, &server, &["lead", "chat", "Propose a hold on PR #44"]);
    assert!(output.status.success(), "{output:?}");
    let reply = String::from_utf8_lossy(&output.stdout);
    assert!(reply.contains("Proposal awaiting owner confirmation"));
    assert!(
        reply.contains(r#""scope":{"kind":"pull_request","value":44}"#),
        "{reply}"
    );
    assert!(reply.contains(r#""kind":"session_entry""#), "{reply}");
    let requests = server.requests();
    let last = requests
        .iter()
        .rev()
        .find(|request| request.max_tokens > 0)
        .unwrap();
    assert!(last.messages.iter().any(|message| {
        message.content.as_deref().is_some_and(|text| {
            text.contains(r#"{"kind":"pull_request","value":42}"#)
                && text.contains(r#"{"kind":"constraint","value":{"active":true,"until":null}}"#)
        })
    }));

    let store = FsStore::new(dirs.sessions_dir);
    let logs = std::fs::read_to_string(store.resolve(id).unwrap().path).unwrap();
    assert!(logs.contains(first) && logs.contains(second));
    assert!(!logs.contains("Observations:"));
}

const LEAD_PTY: &str = r#"
import os, pty, select, signal, subprocess, sys, time
master, slave = pty.openpty()
child = subprocess.Popen([sys.argv[1], 'lead'], stdin=slave, stdout=slave, stderr=slave)
os.close(slave)
output = b''
def wait_for(marker):
    global output
    deadline = time.monotonic() + 15
    while marker not in output:
        assert child.poll() is None, output[-4000:]
        assert time.monotonic() < deadline, output[-4000:]
        if select.select([master], [], [], .1)[0]:
            output += os.read(master, 65536)
try:
    wait_for(b'READY')
    busy = subprocess.run([sys.argv[1], 'lead', '-p', 'Do not overlap.'], capture_output=True, timeout=10)
    assert busy.returncode != 0 and b'busy' in busy.stderr, busy.stderr
    os.write(master, b'/clone\r')
    wait_for(b'attached to its dedicated session')
    os.write(master, b'Terminal follow-up.\r')
    wait_for(b'Interactive reply')
    child.send_signal(signal.SIGHUP)
    assert child.wait(timeout=5) == 129
finally:
    os.close(master)
    if child.poll() is None:
        child.kill()
    child.wait(timeout=5)
"#;

#[test]
fn normal_lead_tui_and_direct_print_use_one_dedicated_session() {
    let test = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::text(r#"{"text":"first reply","facts":[],"action":null}"#),
            ScriptTurn::text("Interactive reply"),
            ScriptTurn::text(r#"{"text":"continued reply","facts":[],"action":null}"#),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    std::fs::set_permissions(test.agent_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let created = run(&test, &server, &["lead", "chat", "--init"]);
    assert!(created.status.success(), "{created:?}");
    let id = String::from_utf8(created.stdout).unwrap();
    let config = serde_json::json!({
        "oxa": {"lead":{"sessionId":id.trim()},"engine":{"inProcessCloud":false,"inProcessLocal":false}},
        "cacheWarming":"off",
        "retry":{"enabled":false},
    });
    let path = test.write("home/.oxa/agent/settings.json", &config.to_string());
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let headless = run(&test, &server, &["lead"]);
    assert!(!headless.status.success());
    assert!(String::from_utf8_lossy(&headless.stderr).contains("needs a terminal"));
    let direct = run(&test, &server, &["lead", "Open the conversation."]);
    assert!(direct.status.success(), "{direct:?}");
    let tui = Command::new("python3")
        .args(["-I", "-c", LEAD_PTY, env!("CARGO_BIN_EXE_oxa")])
        .current_dir(test.work())
        .env_clear()
        .envs(test.envs(&server.url()))
        .env("TERM", "xterm-256color")
        .output()
        .unwrap();
    assert!(tui.status.success(), "{}", String::from_utf8_lossy(&tui.stderr));
    let printed = run(&test, &server, &["lead", "-p", "Continue after the terminal."]);
    assert!(printed.status.success(), "{printed:?}");
    assert!(String::from_utf8_lossy(&printed.stdout).contains("continued reply"));
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].messages.iter().any(|msg| msg
        .content
        .as_deref()
        .is_some_and(|text| text.contains("Open the conversation."))));
    assert!(requests[2].messages.iter().any(|msg| msg
        .content
        .as_deref()
        .is_some_and(|text| text.contains("Terminal follow-up."))));
    assert!(requests.iter().all(|request| request.tools_json == "[]"));
    let sessions = FsStore::new(test.agent_dir().join("sessions")).list(None);
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].sref.id, id.trim());
}
