//! The installed menu needs no runtime; reserved children stay unavailable.
use oxa_fake::{FakeOpts, FakeServer, TestEnv};
use std::process::Command;

fn command(test: &TestEnv) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .env_clear()
        .envs(test.envs("http://127.0.0.1:1"))
        .env_remove("HOME")
        .current_dir(test.work());
    command
}

#[test]
fn menu_and_help_precede_runtime_and_reserved_children_refuse() {
    let test = TestEnv::new();
    for args in [vec!["lead"], vec!["lead", "--help"], vec!["help", "lead"]] {
        let output = command(&test).args(&args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("lead"));
        assert_eq!(text.contains("lead serve"), args.as_slice() == ["lead"]);
    }
    for child in ["board", "status", "asks", "chat"] {
        let output = command(&test).args(["lead", child]).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unavailable"));
    }
}

#[test]
fn offline_menu_precedes_home_and_runtime() {
    let test = TestEnv::new();
    for args in [vec!["--offline", "lead"], vec!["--offline", "--offline", "lead"]] {
        let output = command(&test).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Open the lead command menu"));
    }
}

#[test]
fn legacy_shadow_keeps_its_read_only_resource_floor() {
    for text in [
        "lead --shadow task",
        "lead --model synthetic --shadow task",
        "lead task --shadow",
    ] {
        let words = text.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        let args = oxa_cli::parse_args(&words, &[]).unwrap();
        assert!(args.lead && args.shadow);
        assert!(args.no_tools && args.no_mcp && args.no_extensions && args.no_web_search);
        assert_eq!(args.tools.unwrap(), ["read", "grep", "find", "ls"]);
    }
}

#[test]
fn chat_status_reads_the_protected_board_without_calling_the_provider() {
    use std::{fs, os::unix::fs::PermissionsExt};

    let test = TestEnv::new();
    let directory = test.agent_dir().join("lead-fixture");
    fs::create_dir_all(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let sample = directory.join("board.jsonl");
    let board = oxa_work::board::Board {
        observed_at: 1234,
        entries: vec![oxa_work::board::Entry {
            number: 42,
            head: "a".repeat(40),
            release_pinned: false,
            action: oxa_work::board::Action::Ready,
            lease: None,
            attempts_today: 0,
            eligible: true,
        }],
        ..Default::default()
    };
    fs::write(
        &sample,
        format!(
            "{}\n",
            serde_json::json!({"schema_version":1,"observed_at":1234,"board":board})
        ),
    )
    .unwrap();
    fs::set_permissions(&sample, fs::Permissions::from_mode(0o600)).unwrap();
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let output = command(&test)
        .envs(test.envs(&server.url()))
        .env("OXA_LEAD_BOARD_LOG", &sample)
        .args(["lead", "chat", "status"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply = String::from_utf8(output.stdout).unwrap();
    assert!(reply.contains("1 item"), "{reply}");
    assert!(reply.contains("1234"), "{reply}");
    assert!(reply.contains("0 tokens"), "{reply}");
    assert!(server.requests().is_empty());
}

#[test]
fn fixed_chat_reads_runner_observations_and_handles_missing_data() {
    use oxa_proto::work::Snapshot;
    use std::{fs, os::unix::fs::PermissionsExt};

    let test = TestEnv::new();
    let dir = test.agent_dir().join("runner-fixture");
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let sample = dir.join("situation.json");
    let snapshot = include_str!("../../oxa-proto/tests/fixtures/work/evening.jsonl")
        .lines()
        .find_map(|line| Snapshot::from_json(line.as_bytes()).ok())
        .unwrap();
    fs::write(&sample, snapshot.to_json().unwrap()).unwrap();
    fs::set_permissions(&sample, fs::Permissions::from_mode(0o600)).unwrap();
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    for (prompt, text) in [
        ("workers", format!("{} running tasks", snapshot.running.len())),
        (
            "attention",
            format!("{} open owner decisions", snapshot.asks.len()),
        ),
    ] {
        let output = command(&test)
            .envs(test.envs(&server.url()))
            .env("OXA_LEAD_SITUATION", &sample)
            .args(["lead", "chat", prompt])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let reply = String::from_utf8(output.stdout).unwrap();
        assert!(reply.contains(&text), "{reply}");
        assert!(
            reply.contains(&format!("observed at {}", snapshot.at_ms / 1000)),
            "{reply}"
        );
    }
    let output = command(&test)
        .envs(test.envs(&server.url()))
        .args(["lead", "chat", "attention"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("observations unavailable"));
    assert!(server.requests().is_empty());
}
