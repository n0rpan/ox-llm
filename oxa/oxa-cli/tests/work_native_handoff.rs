//! A later native board attempt continues the branch that a finished attempt left checked out.
#[path = "common/work_repo.rs"]
mod work_repo;
use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use work_repo::{git, setup};

const CHILD: &str = "OXA_NATIVE_HANDOFF_TEST_ROOT";
const BRANCH: &str = "agent/native";
const PR_BRANCH: &str = "agent/reviewed";

/// One native admission: each lease generation has its own task id.
fn attempt(ordinal: u32, branch: &str, base: &str, reviewed: Option<(u64, &str)>) -> oxa_work::Task {
    let lease = oxa_work::board::Lease {
        holder: "codex:node/slot-1".into(),
        expires_at: 2_000_000_000,
        claimed_at: Some(1_000_000_000),
        generation: (20_000, ordinal),
    };
    let binding = oxa_work::board::NativeBinding::new(7, &"a".repeat(64), &lease, branch).unwrap();
    let mut task = serde_json::json!({
        "id": "i7", "model": "fake", "branch": branch, "base": base, "prompt": "Synthetic native task"
    });
    if let Some((number, head)) = reviewed {
        task["reviewed_pull_request"] = serde_json::json!({"number": number, "head": head});
    }
    oxa_work::native_task(&binding, serde_json::from_value(task).unwrap()).unwrap()
}

fn policy(root: &Path) -> oxa_work::RunPolicy {
    oxa_work::RunPolicy {
        git_binary: "/usr/bin/git".into(),
        host_path: std::env::var_os("PATH").unwrap(),
        terminate_binary: Some("/usr/bin/kill".into()),
        runner_owns_github: false,
        offline: false,
        storage: root.join("home/.oxa/agent/work/native-handoff"),
    }
}

fn read(project: &Path, args: &[&str]) -> String {
    let out = Command::new("/usr/bin/git")
        .current_dir(project)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn run(project: &Path, policy: &oxa_work::RunPolicy, task: oxa_work::Task) -> oxa_work::Record {
    let binary = Path::new(env!("CARGO_BIN_EXE_oxa"));
    let plan = oxa_work::plan_task(project, task, policy).unwrap();
    oxa_work::run_task_with_policy(project, binary, &plan, policy).unwrap()
}

fn branch_attempts(root: PathBuf) {
    let project = root.join("work/project");
    let policy = policy(&root);
    let mut records = vec![];
    for ordinal in [1, 2] {
        let record = run(&project, &policy, attempt(ordinal, BRANCH, "main", None));
        assert_eq!(record.outcome, oxa_work::Outcome::Done, "{record:?}");
        records.push(record);
    }
    assert_ne!(records[0].id, records[1].id);
    assert_ne!(records[0].worktree, records[1].worktree);
    assert_eq!(
        read(&project, &["show", &format!("{BRANCH}:first.txt")]),
        "first attempt"
    );
    assert_eq!(
        read(&project, &["show", &format!("{BRANCH}:second.txt")]),
        "second attempt"
    );
    // The finished attempt keeps its files; only its HEAD was detached.
    assert!(records[0].worktree.join("first.txt").exists());
}

fn reviewed_attempts(root: PathBuf) {
    let project = root.join("work/project");
    let policy = policy(&root);
    let base = read(&project, &["rev-parse", "main"]);
    let tip = |project: &Path| read(project, &["rev-parse", &format!("refs/heads/{PR_BRANCH}")]);
    let first_head = tip(&project);
    let first = run(
        &project,
        &policy,
        attempt(1, PR_BRANCH, &base, Some((42, &first_head))),
    );
    assert_eq!(first.outcome, oxa_work::Outcome::Done, "{first:?}");
    // The reviewed head moves with the saved work; the pull request number does not.
    let second_head = tip(&project);
    assert_ne!(second_head, first_head);
    let second = run(
        &project,
        &policy,
        attempt(2, PR_BRANCH, &base, Some((42, &second_head))),
    );
    assert_eq!(second.outcome, oxa_work::Outcome::Done, "{second:?}");
    assert_ne!(first.id, second.id);
    assert_ne!(first.worktree, second.worktree);
    assert!(first.worktree.join("first.txt").exists());
    let other_head = tip(&project);
    let other = run(
        &project,
        &policy,
        attempt(3, PR_BRANCH, &base, Some((43, &other_head))),
    );
    assert_eq!(other.outcome, oxa_work::Outcome::Blocked, "{other:?}");
    assert!(
        other
            .detail
            .contains("outside a finished task for this pull request"),
        "{other:?}"
    );
    assert_eq!(tip(&project), other_head);
}

fn turns() -> Vec<ScriptTurn> {
    let write = |path: &str, content: &str| {
        let args = serde_json::json!({"path": path, "content": content}).to_string();
        [
            ScriptTurn::tool("write", "write", &args),
            ScriptTurn::text("done"),
        ]
    };
    let mut turns = write("first.txt", "first attempt").to_vec();
    turns.extend(write("second.txt", "second attempt"));
    turns
}

fn in_child(test: &str, env: &TestEnv) {
    let server = FakeServer::start(turns(), FakeOpts::default()).unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test])
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

#[test]
fn a_later_native_attempt_continues_the_branch_of_a_finished_attempt() {
    if let Some(root) = std::env::var_os(CHILD) {
        branch_attempts(PathBuf::from(root));
        return;
    }
    let env = TestEnv::new();
    setup(&env, "[]");
    in_child(
        "a_later_native_attempt_continues_the_branch_of_a_finished_attempt",
        &env,
    );
}

#[test]
fn native_attempts_on_one_reviewed_pull_request_hand_over_its_branch() {
    if let Some(root) = std::env::var_os(CHILD) {
        reviewed_attempts(PathBuf::from(root));
        return;
    }
    let env = TestEnv::new();
    setup(&env, "[]");
    let project = env.work().join("project");
    git(&env, &project, &["checkout", "-q", "-b", PR_BRANCH]);
    env.write("work/project/reviewed.txt", "pull request change\n");
    git(&env, &project, &["add", "reviewed.txt"]);
    git(&env, &project, &["commit", "-q", "-m", "pull request change"]);
    git(&env, &project, &["checkout", "-q", "main"]);
    in_child(
        "native_attempts_on_one_reviewed_pull_request_hand_over_its_branch",
        &env,
    );
}
