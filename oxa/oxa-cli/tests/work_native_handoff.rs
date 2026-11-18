//! A later native board attempt continues the branch that a finished attempt left checked out.
#[path = "common/work_repo.rs"]
mod work_repo;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::{path::PathBuf, process::Command};
use work_repo::setup;

const CHILD: &str = "OXA_NATIVE_HANDOFF_TEST_ROOT";
const BRANCH: &str = "agent/native";

fn attempt(ordinal: u32) -> oxa_work::Task {
    let lease = oxa_work::board::Lease {
        holder: "codex:node/slot-1".into(),
        expires_at: 2_000_000_000,
        claimed_at: Some(1_000_000_000),
        generation: (20_000, ordinal),
    };
    let binding = oxa_work::board::NativeBinding::new(7, &"a".repeat(64), &lease, BRANCH).unwrap();
    let task: oxa_work::Task = serde_json::from_value(serde_json::json!({
        "id": "i7", "model": "fake", "branch": BRANCH, "base": "main", "prompt": "Synthetic native task"
    }))
    .unwrap();
    oxa_work::native_task(&binding, task).unwrap()
}

fn run_attempts(root: PathBuf) {
    let project = root.join("work/project");
    let policy = oxa_work::RunPolicy {
        git_binary: "/usr/bin/git".into(),
        host_path: std::env::var_os("PATH").unwrap(),
        terminate_binary: Some("/usr/bin/kill".into()),
        runner_owns_github: false,
        offline: false,
        storage: root.join("home/.oxa/agent/work/native-handoff"),
    };
    let binary = std::path::Path::new(env!("CARGO_BIN_EXE_oxa"));
    let mut records = vec![];
    for ordinal in [1, 2] {
        let task = attempt(ordinal);
        let plan = oxa_work::plan_task(&project, task, &policy).unwrap();
        let record = oxa_work::run_task_with_policy(&project, binary, &plan, &policy).unwrap();
        assert_eq!(record.outcome, oxa_work::Outcome::Done, "{record:?}");
        records.push(record);
    }
    assert_ne!(records[0].id, records[1].id);
    assert_ne!(records[0].worktree, records[1].worktree);
    let show = |rev: &str| {
        let out = Command::new("/usr/bin/git")
            .current_dir(&project)
            .args(["show", rev])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(show(&format!("{BRANCH}:first.txt")), "first attempt");
    assert_eq!(show(&format!("{BRANCH}:second.txt")), "second attempt");
    // The finished attempt keeps its files; only its HEAD was detached.
    assert!(records[0].worktree.join("first.txt").exists());
}

#[test]
fn a_later_native_attempt_continues_the_branch_of_a_finished_attempt() {
    if let Some(root) = std::env::var_os(CHILD) {
        run_attempts(PathBuf::from(root));
        return;
    }
    let env = TestEnv::new();
    setup(&env, "[]");
    let write = |path: &str, content: &str| {
        let args = serde_json::json!({"path": path, "content": content}).to_string();
        [
            ScriptTurn::tool("write", "write", &args),
            ScriptTurn::text("done"),
        ]
    };
    let mut turns = write("first.txt", "first attempt").to_vec();
    turns.extend(write("second.txt", "second attempt"));
    let server = FakeServer::start(turns, FakeOpts::default()).unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_later_native_attempt_continues_the_branch_of_a_finished_attempt",
        ])
        .env_clear()
        .envs(env.envs(&server.url()))
        .env(CHILD, env.root())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{} / {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
