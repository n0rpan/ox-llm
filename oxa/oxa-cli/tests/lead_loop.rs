use oxa_config::Dirs;
use oxa_fake::{FakeOpts, FakeServer, TestEnv};
use oxa_session::{FsStore, Goal, SessionStore};
use oxa_work::board::{Action, Board, Entry};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    process::{Command, Stdio},
};

fn write_board(path: &Path, observed_at: u64, rows: &[(u64, Action, bool)]) {
    let board = Board {
        observed_at,
        entries: rows
            .iter()
            .map(|&(number, action, eligible)| Entry {
                number,
                head: "a".repeat(40),
                release_pinned: false,
                action,
                lease: None,
                attempts_today: 0,
                eligible,
            })
            .collect(),
    };
    fs::write(
        path,
        format!(
            "{}\n",
            json!({"schema_version":1,"observed_at":observed_at,"board":board})
        ),
    )
    .unwrap();
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
    fs::write(
        dirs.settings_file(),
        json!({"oxa":{"lead":{"sessionId":id}}}).to_string(),
    )
    .unwrap();
    fs::set_permissions(dirs.settings_file(), fs::Permissions::from_mode(0o600)).unwrap();
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

    assert!(command().output().unwrap().status.success());
    assert_eq!(Goal::revision(&store.load(&session).unwrap()), 0);
    write_board(&board, 1, &[(42, Action::Ready, true)]);
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
            (8, Action::FixFindings, false),
            (42, Action::Ready, true),
            (66, Action::FixRequiredCi, true),
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

    write_board(&board, now, &[(12, Action::BuildIssue, true)]);
    for args in [vec!["run", "--once"], vec!["asks"]] {
        let reopened = command().args(args).output().unwrap();
        assert!(reopened.status.success(), "{reopened:?}");
        let reopened: Value = serde_json::from_slice(&reopened.stdout).unwrap();
        assert_eq!(reopened["goal_revision"], first["goal_revision"]);
        assert_eq!(reopened["asks"], first["asks"]);
    }
    let saved = Goal::latest(&store.load(&session).unwrap()).unwrap().unwrap();
    assert!(saved.lead.unwrap().held_leases.is_empty());
    assert!(server.requests().is_empty());
}
