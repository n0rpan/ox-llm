use oxa_config::Dirs;
use oxa_fake::{FakeOpts, FakeServer, TestEnv};
use oxa_session::{FsStore, Goal, SessionStore};
use oxa_work::board::{Action, Board, Entry, Lease};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    process::{Command, Stdio},
};

fn write_board(path: &Path, observed_at: u64, rows: &[(u64, Action, bool, Option<u64>)]) {
    let board = Board {
        observed_at,
        entries: rows
            .iter()
            .map(|&(number, action, eligible, expires_at)| Entry {
                number,
                head: "a".repeat(40),
                release_pinned: false,
                action,
                lease: expires_at.map(|expires_at| Lease {
                    holder: "fixture".into(),
                    expires_at,
                    claimed_at: Some(expires_at - 1),
                    generation: (expires_at / 86_400, 1),
                }),
                attempts_today: 0,
                eligible,
            })
            .collect(),
    };
    let sample = json!({"schema_version":1,"observed_at":observed_at,"board":board});
    fs::write(path, format!("{sample}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn running_lead_reopens_the_same_owner_ask_after_process_kill() {
    let test = TestEnv::new();
    let dirs = Dirs::under(test.root());
    fs::set_permissions(&dirs.agent_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let uid = fs::metadata(&dirs.agent_dir).unwrap().uid();
    let project = test.work().canonicalize().unwrap();
    let id = oxa_cli::LeadOwnerBinding::create(&dirs, &project, uid).unwrap();
    let settings = test.write(
        "home/.oxa/agent/settings.json",
        &json!({"oxa":{"lead":{"sessionId":id}}}).to_string(),
    );
    fs::set_permissions(settings, fs::Permissions::from_mode(0o600)).unwrap();
    let store = FsStore::new(dirs.sessions_dir.clone());
    let session = store.resolve(&id).unwrap();
    let board = dirs.agent_dir.join("board.jsonl");
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
        command
            .env_clear()
            .envs(test.envs(&server.url()))
            .current_dir(&project)
            .env("OXA_LEAD_BOARD_LOG", &board)
            .args(["--offline", "lead"]);
        command
    };

    let help = command().arg("--help").output().unwrap();
    assert!(help.status.success(), "{help:?}");
    write_board(&board, 1, &[(42, Action::Ready, true, None)]);
    let agent = command()
        .env("AI_AGENT", "")
        .args(["run", "--once"])
        .output()
        .unwrap();
    assert!(!agent.status.success());
    assert_eq!(Goal::revision(&store.load(&session).unwrap()), 0);
    let stale = command().args(["run", "--once"]).output().unwrap();
    assert!(stale.status.success(), "{stale:?}");
    let stale: Value = serde_json::from_slice(&stale.stdout).unwrap();
    assert_eq!(stale["state"], "board_unavailable");
    assert_eq!(stale["asks"], json!([]));

    let now = oxa_proto::time::now_ms() / 1000;
    write_board(
        &board,
        now,
        &[
            (8, Action::FixFindings, false, Some(now + 60)),
            (42, Action::Ready, true, None),
            (66, Action::FixRequiredCi, true, Some(now)),
        ],
    );
    let mut running = command().arg("run").stdout(Stdio::piped()).spawn().unwrap();
    let mut first = String::new();
    BufReader::new(running.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    running.kill().unwrap();
    assert!(!running.wait().unwrap().success());
    let first: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first["state"], "awaiting_owner");
    assert_eq!(first["asks"].as_array().unwrap().len(), 1);
    assert_eq!(first["asks"][0]["id"], "lead:board:66");

    write_board(&board, now, &[(12, Action::BuildIssue, true, None)]);
    assert!(first["asks"][0].get("question").is_none());
    let reopened = command().args(["run", "--once"]).output().unwrap();
    assert!(reopened.status.success(), "{reopened:?}");
    let reopened: Value = serde_json::from_slice(&reopened.stdout).unwrap();
    assert_eq!(reopened["goal_revision"], first["goal_revision"]);
    assert_eq!(reopened["asks"], first["asks"]);

    let asks = command().arg("asks").output().unwrap();
    assert!(asks.status.success(), "{asks:?}");
    let asks: Value = serde_json::from_slice(&asks.stdout).unwrap();
    assert_eq!(asks["goal_revision"], first["goal_revision"]);
    assert_eq!(asks["asks"][0]["id"], first["asks"][0]["id"]);
    assert!(asks["asks"][0]["question"].as_str().unwrap().contains("#66"));
    let saved = Goal::latest(&store.load(&session).unwrap()).unwrap().unwrap();
    assert!(saved.lead.unwrap().held_leases.is_empty());
    assert!(server.requests().is_empty());
}

#[test]
fn a_missing_protected_board_observation_keeps_the_saved_ask_and_names_the_cause() {
    let test = TestEnv::new();
    let dirs = Dirs::under(test.root());
    fs::set_permissions(&dirs.agent_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let uid = fs::metadata(&dirs.agent_dir).unwrap().uid();
    let project = test.work().canonicalize().unwrap();
    let id = oxa_cli::LeadOwnerBinding::create(&dirs, &project, uid).unwrap();
    let settings = test.write(
        "home/.oxa/agent/settings.json",
        &json!({"oxa":{"lead":{"sessionId":id}}}).to_string(),
    );
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o600)).unwrap();
    let store = FsStore::new(dirs.sessions_dir.clone());
    let session = store.resolve(&id).unwrap();
    let board = dirs.agent_dir.join("board.jsonl");
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
        command
            .env_clear()
            .envs(test.envs(&server.url()))
            .current_dir(&project)
            .env("OXA_LEAD_BOARD_LOG", &board)
            .args(["--offline", "lead"]);
        command
    };

    // A fresh observation saves one owner ask in the protected Lead session.
    let now = oxa_proto::time::now_ms() / 1000;
    write_board(&board, now, &[(42, Action::Ready, true, None)]);
    let fresh = command().args(["run", "--once"]).output().unwrap();
    assert!(fresh.status.success(), "{fresh:?}");
    let fresh: Value = serde_json::from_slice(&fresh.stdout).unwrap();
    assert_eq!(fresh["state"], "awaiting_owner");
    assert_eq!(fresh["asks"][0]["id"], "lead:board:42");
    let saved = Goal::latest(&store.load(&session).unwrap()).unwrap().unwrap();
    assert_eq!(saved.lead.unwrap().open_asks.len(), 1);

    // The protected observation is now missing: the saved ask must survive
    // and the observation must report an actionable, path-free cause.
    fs::remove_file(&board).unwrap();
    let missing = command().args(["run", "--once"]).output().unwrap();
    assert!(missing.status.success(), "{missing:?}");
    let missing: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(missing["asks"][0]["id"], "lead:board:42");
    assert_eq!(
        missing["cause"].as_str(),
        Some("the protected Board observation is missing"),
        "{missing}"
    );
    assert_eq!(
        missing["hint"].as_str(),
        Some("write a fresh protected Board observation and rerun the Lead loop"),
        "{missing}"
    );
    assert!(!missing.to_string().contains(&*dirs.agent_dir.to_string_lossy()));
    let saved = Goal::latest(&store.load(&session).unwrap()).unwrap().unwrap();
    assert_eq!(saved.lead.unwrap().open_asks.len(), 1);
    assert!(server.requests().is_empty());
}
