//! #1442 through the real runner, worker and sandbox: the done checks derived from the
//! change's own crates, the worker's declared acceptance crates, honest `unverified`
//! outcomes when a check cannot run, and the per-check evidence a `done` rests on. The
//! cargo on the worker PATH is a recording fake; no build toolchain is used, no network.
#[path = "common/work.rs"]
mod work;

use oxa_fake::{ScriptTurn, TestEnv};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};
use work::{git, run_args, run_observed, run_with_exit, setup, setup_with_files};

const TEST_ENTRY: &str = "in .: command -v cargo >/dev/null 2>&1 || exit 127\ncargo test --frozen -p one";
const CLIPPY_ENTRY: &str =
    "in .: cargo clippy --version >/dev/null 2>&1 || exit 127\ncargo clippy --frozen -p one -- -D warnings";

/// The base fixture: a Cargo workspace with two committed member crates.
fn crate_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/one\", \"crates/two\"]\n",
        ),
        ("crates/one/Cargo.toml", "[package]\nname = \"one\"\n"),
        ("crates/one/src/lib.rs", "pub fn one() {}\n"),
        ("crates/two/Cargo.toml", "[package]\nname = \"two\"\n"),
        ("crates/two/src/lib.rs", "pub fn two() {}\n"),
    ]
}

fn write_turn(path: &str) -> ScriptTurn {
    ScriptTurn::tool(
        "c1",
        "write",
        &json!({"path": path, "content": "changed"}).to_string(),
    )
}

/// A private tools directory holding a `cargo` that runs `script`. It lives under the
/// test home: PATH entries under `$HOME` are bound read-only into the check sandbox
/// (a fresh tmpfs masks the host `/tmp` there), so the sandboxed round resolves the fake.
fn tools_with_cargo_in(env: &TestEnv, name: &str, script: &str) -> PathBuf {
    let tools = env.home().join(name);
    fs::DirBuilder::new().mode(0o700).create(&tools).unwrap();
    let cargo = tools.join("cargo");
    fs::write(&cargo, script).unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o700)).unwrap();
    tools
}

fn tools_with_cargo(env: &TestEnv, script: &str) -> PathBuf {
    tools_with_cargo_in(env, "trusted-tools", script)
}

/// The recording, always-passing cargo: every invocation and its target directory.
const RECORDING_CARGO: &str = "#!/bin/sh
printf '%s|target=%s\\n' \"$*\" \"$CARGO_TARGET_DIR\" >> \"$OXA_WORK_OUTPUT_DIR/calls\"
exit 0
";

/// A cargo that fails its very first invocation and passes afterwards.
const ONCE_FAILING_CARGO: &str = "#!/bin/sh
printf '%s\\n' \"$*\" >> \"$OXA_WORK_OUTPUT_DIR/calls\"
if [ ! -f \"$OXA_WORK_OUTPUT_DIR/first\" ]; then
  touch \"$OXA_WORK_OUTPUT_DIR/first\"
  exit 1
fi
exit 0
";

fn path_with(tools: &Path) -> String {
    format!(
        "{}:{}",
        tools.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn verify(env: &TestEnv, path: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .args(["work", "verify", "test"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"));
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.output().unwrap()
}

/// The per-check calls the fake cargo recorded beside the task's worktree.
fn calls(tree: &Path) -> String {
    fs::read_to_string(
        tree.parent()
            .unwrap()
            .join(".work-output")
            .join("test")
            .join("calls"),
    )
    .unwrap()
}

fn tree_of(record: &serde_json::Value) -> &Path {
    Path::new(record["worktree"].as_str().unwrap())
}

#[test]
fn the_touched_crate_tests_and_clippy_join_the_static_list_in_the_done_evidence() {
    let env = TestEnv::new();
    setup_with_files(&env, r#"["true"]"#, &crate_files());
    let tools = tools_with_cargo(&env, RECORDING_CARGO);
    let record = run_args(
        &env,
        vec![write_turn("crates/one/src/lib.rs"), ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        Some(path_with(&tools)),
    );
    assert_eq!(record["outcome"], "done", "{record}");
    // The done record lists every check it rests on: the frozen static list, then the
    // tests and clippy of the one crate the change touched, each with its exit status.
    assert_eq!(
        record["checks"],
        json!([
            {"origin": "settings", "command": "true", "exit": 0, "timed_out": false, "status": "passed"},
            {"origin": "derived", "command": TEST_ENTRY, "exit": 0, "timed_out": false, "status": "passed"},
            {"origin": "derived", "command": CLIPPY_ENTRY, "exit": 0, "timed_out": false, "status": "passed"},
        ]),
        "{record}"
    );
    let tree = tree_of(&record);
    let target = tree.parent().unwrap().join(".work-output/test/target");
    let calls = calls(tree);
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        vec![
            format!("test --frozen -p one|target={}", target.display()),
            format!("clippy --version|target={}", target.display()),
            format!(
                "clippy --frozen -p one -- -D warnings|target={}",
                target.display()
            ),
        ],
        "{calls}"
    );
    // The crate the worker did not touch is not checked.
    assert!(!calls.contains(" -p two"));
}

#[test]
fn a_check_that_never_ran_leaves_the_task_unverified_and_the_runner_failing() {
    let env = TestEnv::new();
    setup(&env, r#"["exit 127"]"#);
    let (records, _, code) = run_with_exit(
        &env,
        vec![ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        None,
    );
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record["outcome"], "unverified", "{record}");
    assert_eq!(code, 1, "an unverified task is not a success exit");
    assert_eq!(
        record["checks"],
        json!([
            {"origin": "settings", "command": "exit 127", "exit": 127, "timed_out": false, "status": "not_run"},
        ]),
        "{record}"
    );
    assert!(
        record["detail"].as_str().unwrap().contains("exit 127 (exit 127)"),
        "{}",
        record["detail"]
    );
}

#[test]
fn a_declared_acceptance_crate_runs_beside_the_derived_checks() {
    let env = TestEnv::new();
    setup_with_files(&env, "[]", &crate_files());
    let tools = tools_with_cargo(&env, RECORDING_CARGO);
    let record = run_args(
        &env,
        vec![
            write_turn("crates/one/src/lib.rs"),
            ScriptTurn::text("finished <oxa-acceptance>[\"two\"]</oxa-acceptance>"),
        ],
        &["work", "../tasks.json"],
        Some(path_with(&tools)),
    );
    assert_eq!(record["outcome"], "done", "{record}");
    // The untouched crate the worker named still has its tests gate completion.
    assert_eq!(
        record["checks"],
        json!([
            {"origin": "derived", "command": TEST_ENTRY, "exit": 0, "timed_out": false, "status": "passed"},
            {"origin": "derived", "command": CLIPPY_ENTRY, "exit": 0, "timed_out": false, "status": "passed"},
            {
                "origin": "declared",
                "command": "in .: command -v cargo >/dev/null 2>&1 || exit 127\ncargo test --frozen -p two",
                "exit": 0, "timed_out": false, "status": "passed"
            },
        ]),
        "{record}"
    );
    let calls = calls(tree_of(&record));
    assert_eq!(
        calls
            .lines()
            .map(|line| line.split('|').next().unwrap())
            .collect::<Vec<_>>(),
        [
            "test --frozen -p one",
            "clippy --version",
            "clippy --frozen -p one -- -D warnings",
            "test --frozen -p two"
        ]
    );
}

#[test]
fn a_declared_crate_that_is_not_of_the_repository_keeps_the_task_unverified() {
    let env = TestEnv::new();
    setup_with_files(&env, "[]", &[]);
    let record = run_args(
        &env,
        vec![
            write_turn("notes.txt"),
            ScriptTurn::text("finished <oxa-acceptance>[\"missing\"]</oxa-acceptance>"),
        ],
        &["work", "../tasks.json"],
        None,
    );
    assert_eq!(record["outcome"], "unverified", "{record}");
    assert!(
        record["detail"]
            .as_str()
            .unwrap()
            .contains("the declared acceptance crate missing is not a crate of this repository"),
        "{}",
        record["detail"]
    );
    // Nothing could run, so no check is recorded as evidence, and the work is retained.
    assert!(record.get("checks").is_none(), "{record}");
    assert_eq!(
        git(&env, tree_of(&record), &["show", "HEAD:notes.txt"]),
        "changed"
    );
}

#[test]
fn a_failing_derived_check_gets_one_repair_round_over_the_same_session() {
    let env = TestEnv::new();
    setup_with_files(&env, "[]", &crate_files());
    let tools = tools_with_cargo(&env, ONCE_FAILING_CARGO);
    let (record, requests) = run_observed(
        &env,
        vec![
            write_turn("crates/one/src/lib.rs"),
            ScriptTurn::text("finished"),
            ScriptTurn::text("repaired"),
        ],
        &["work", "../tasks.json"],
        Some(path_with(&tools)),
    );
    assert_eq!(record["outcome"], "done", "{record}");
    // The repair prompt carries the derived failure and names the check that failed.
    assert_eq!(requests.len(), 3, "worker, then one repair over the same session");
    assert!(
        requests[2].messages.iter().any(|message| message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("External acceptance checks failed")
                && text.contains("cargo test --frozen -p one"))),
        "repair prompt: {:?}",
        requests[2].messages.last().and_then(|m| m.content.as_deref())
    );
    // The failing round ran test then clippy; the recheck ran both again.
    let calls = calls(tree_of(&record));
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        [
            "test --frozen -p one",
            "clippy --version",
            "clippy --frozen -p one -- -D warnings",
            "test --frozen -p one",
            "clippy --version",
            "clippy --frozen -p one -- -D warnings",
        ]
    );
    // The recorded evidence is the passing recheck, with nothing hidden.
    for check in record["checks"].as_array().unwrap() {
        assert_eq!(check["status"], "passed", "{record}");
    }
}

#[test]
fn verification_reruns_the_derived_checks_of_the_saved_commit() {
    let env = TestEnv::new();
    setup_with_files(&env, "[]", &crate_files());
    let tools = tools_with_cargo(&env, RECORDING_CARGO);
    let record = run_args(
        &env,
        vec![write_turn("crates/one/src/lib.rs"), ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        Some(path_with(&tools)),
    );
    assert_eq!(record["outcome"], "done", "{record}");

    let failing = tools_with_cargo_in(
        &env,
        "failing-tools",
        "#!/bin/sh\nif [ \"$*\" = \"clippy --version\" ]; then exit 0; fi\nprintf '%s\\n' \"$*\" >> \"$OXA_WORK_OUTPUT_DIR/calls\"\nexit 1\n",
    );
    let rejected = verify(&env, Some(&path_with(&failing)));
    assert!(
        !rejected.status.success(),
        "a committed change whose crate tests fail still verifies: {}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("clean committed acceptance failed"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    let accepted: Output = verify(&env, Some(&path_with(&tools)));
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
}

#[test]
fn a_changed_task_with_no_checks_is_unverified_and_cannot_get_a_verify_receipt() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let (records, _, code) = run_with_exit(
        &env,
        vec![write_turn("notes.txt"), ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        None,
    );
    assert_eq!(code, 1, "{records:?}");
    assert_eq!(records[0]["outcome"], "unverified");
    assert!(records[0]["detail"]
        .as_str()
        .unwrap()
        .contains("no acceptance checks"));
    assert_eq!(
        git(&env, tree_of(&records[0]), &["show", "HEAD:notes.txt"]),
        "changed"
    );

    let common = env.work().join("project/.git").canonicalize().unwrap();
    let key = format!("{:x}", Sha256::digest(common.as_os_str().as_encoded_bytes()));
    let saved = env.agent_dir().join("work").join(key).join("test.json");
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&saved).unwrap()).unwrap();
    assert_ne!(state["execution"]["initial"], records[0]["head_sha"]);
    state["record"]["outcome"] = json!("done");
    fs::write(saved, serde_json::to_vec(&state).unwrap()).unwrap();

    let output = verify(&env, None);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty(), "zero checks must not produce a receipt");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("no acceptance checks"), "{error}");

    let (replayed, requests, code) = run_with_exit(&env, vec![], &["work", "../tasks.json"], None);
    assert_eq!(code, 1, "{replayed:?}");
    assert_eq!(replayed[0]["outcome"], "unverified");
    assert!(
        requests.is_empty(),
        "replaying a legacy record must not start a worker"
    );
}

#[test]
fn a_clean_task_without_checks_is_also_unverified() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let (records, _, code) = run_with_exit(
        &env,
        vec![ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        None,
    );
    assert_eq!(code, 1, "{records:?}");
    assert_eq!(records[0]["outcome"], "unverified");
    let output = verify(&env, None);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

#[test]
fn a_trust_skipped_round_remains_unverified_even_when_save_cannot_run() {
    use oxa_fake::{FakeOpts, FakeServer, Fault};
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };

    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    let project = env.work().join("project");
    let mut response = ScriptTurn::text("finished");
    response.fault = Some(Fault::Stall { bytes: 16, ms: 2000 });
    let server = FakeServer::start(vec![response], FakeOpts::default()).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(&project)
        .env_clear()
        .envs(env.envs(&server.url()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while server.requests().is_empty() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "worker reached the loopback engine"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    oxa_perm::TrustStore::default()
        .save(&env.agent_dir().join("trust.json"), &project, false)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["outcome"], "unverified", "{record}");
    assert!(record["detail"]
        .as_str()
        .unwrap()
        .contains("source project is not trusted"));
    assert!(
        record.get("checks").is_none(),
        "skipped checks cannot produce evidence"
    );
}
