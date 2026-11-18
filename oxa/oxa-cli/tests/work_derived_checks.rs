//! #1442 through the real runner, worker and sandbox: the done checks derived from the
//! change's own crates, worker declarations, honest `unverified`
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

const CHECK_ENTRY: &str = "in .: cargo --version >/dev/null || exit 127\ncargo check -p one";

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
fn tools_with_cargo(env: &TestEnv, script: &str) -> PathBuf {
    let tools = env.home().join("trusted-tools");
    fs::DirBuilder::new().mode(0o700).create(&tools).unwrap();
    let cargo = tools.join("cargo");
    fs::write(&cargo, script).unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o700)).unwrap();
    tools
}

/// The recording, always-passing cargo: every compile check and its target directory.
const RECORDING_CARGO: &str = "#!/bin/sh
if [ \"$1\" = --version ]; then exit 0; fi
printf '%s|target=%s\\n' \"$*\" \"$CARGO_TARGET_DIR\" >> \"$OXA_WORK_OUTPUT_DIR/calls\"
exit 0
";

/// A cargo that passes discovery but fails its first compile check.
const ONCE_FAILING_CARGO: &str = "#!/bin/sh
if [ \"$1\" = --version ]; then exit 0; fi
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
fn the_supervisor_uses_one_deadline_for_every_check_in_a_round() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let source = env.work().join("project");
    for (deadline, exit, timed_out) in [(oxa_proto::time::now_ms() + 60_000, 0, false), (0, 1, true)] {
        let request = json!({
            "source": &source,
            "operation": {
                "kind": "checks",
                "task": "test-deadline",
                "deadline_at_ms": deadline,
                "commands": ["true", "true # second check"],
            },
        });
        let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(["work", "--supervise", &request.to_string()])
            .current_dir(&source)
            .env_clear()
            .envs(env.envs("http://127.0.0.1:1"))
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(exit),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let check_exit = (!timed_out).then_some(0);
        assert_eq!(
            receipt["checks"],
            json!([
                {"command": "true", "exit": check_exit, "timed_out": timed_out},
                {"command": "true # second check", "exit": check_exit, "timed_out": timed_out},
            ])
        );
    }
}

#[test]
fn local_compile_for_the_touched_crate_is_a_precheck_without_hosted_acceptance() {
    let env = TestEnv::new();
    setup_with_files(&env, r#"["true"]"#, &crate_files());
    let tools = tools_with_cargo(&env, RECORDING_CARGO);
    let record = run_args(
        &env,
        vec![write_turn("crates/one/src/lib.rs"), ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        Some(path_with(&tools)),
    );
    assert_eq!(record["outcome"], "unverified", "{record}");
    // The record keeps the static and compile receipts while hosted test evidence is pending.
    assert_eq!(
        record["checks"],
        json!([
            {"origin": "settings", "command": "true", "exit": 0, "timed_out": false, "status": "passed"},
            {"origin": "derived", "command": CHECK_ENTRY, "exit": 0, "timed_out": false, "status": "passed"},
        ]),
        "{record}"
    );
    let tree = tree_of(&record);
    let target = tree.parent().unwrap().join(".work-output/test/target");
    let calls = calls(tree);
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        [format!("check -p one|target={}", target.display())],
        "{calls}"
    );
    // The crate the worker did not touch is not checked.
    assert!(!calls.contains(" -p two"));
}

#[test]
fn local_compile_commands_come_only_from_owner_settings() {
    let env = TestEnv::new();
    setup_with_files(&env, "[]", &crate_files());
    let project = env.work().join("project");
    env.write(
        "work/project/.oxa/settings.json",
        r#"{"done":[],"oxa":{"work":{"localChecks":["cargo check --offline"]}}}"#,
    );
    git(&env, &project, &["add", ".oxa/settings.json"]);
    git(
        &env,
        &project,
        &["commit", "-m", "Set fixture project check preference"],
    );
    git(&env, &project, &["push", "origin", "main"]);
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"work":{"localChecks":["cargo check --locked"]}}}"#,
    );
    let tools = tools_with_cargo(&env, RECORDING_CARGO);
    let record = run_args(
        &env,
        vec![write_turn("crates/one/src/lib.rs"), ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        Some(path_with(&tools)),
    );
    assert_eq!(record["outcome"], "unverified", "{record}");
    let calls = calls(tree_of(&record));
    assert_eq!(
        calls
            .lines()
            .map(|line| line.split('|').next().unwrap())
            .collect::<Vec<_>>(),
        ["check --locked -p one"]
    );
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
fn a_successful_check_keeps_its_original_command_private() {
    let env = TestEnv::new();
    let fixture_value = "synthetic-check-proof";
    let command = format!("test 'token={fixture_value}' != 'token=<secret>'");
    setup(&env, &json!([command]).to_string());
    let record = run_args(
        &env,
        vec![ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        None,
    );
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(record["checks"][0]["status"], "passed", "{record}");
    assert_eq!(
        record["checks"][0]["command"],
        "test 'token=<secret>' != 'token=<secret>'"
    );
    assert!(!record.to_string().contains(fixture_value));

    let common = env.work().join("project/.git").canonicalize().unwrap();
    let key = format!("{:x}", Sha256::digest(common.as_os_str().as_encoded_bytes()));
    let saved = env.agent_dir().join("work").join(key).join("test.json");
    let checkpoint: serde_json::Value = serde_json::from_slice(&fs::read(saved).unwrap()).unwrap();
    assert_eq!(checkpoint["execution"]["commands"][0], command);
    assert!(!checkpoint["record"].to_string().contains(fixture_value));
    assert!(
        verify(&env, None).status.success(),
        "saved verification uses the original"
    );
}

#[test]
fn a_missing_selected_toolchain_is_not_run_and_does_not_launch_repair() {
    let env = TestEnv::new();
    setup_with_files(&env, "[]", &crate_files());
    let tools = tools_with_cargo(
        &env,
        "#!/bin/sh\n\
         if [ \"$1\" = --version ]; then exit 1; fi\n\
         printf '%s\\n' \"$*\" >> \"$OXA_WORK_OUTPUT_DIR/calls\"\n\
         exit 1\n",
    );
    let (record, requests) = run_observed(
        &env,
        vec![
            write_turn("crates/one/src/lib.rs"),
            ScriptTurn::text("finished"),
            ScriptTurn::text("unexpected repair"),
        ],
        &["work", "../tasks.json"],
        Some(path_with(&tools)),
    );
    assert_eq!(record["outcome"], "unverified", "{record}");
    assert_eq!(requests.len(), 2);
    assert_eq!(record["checks"][0]["origin"], "derived", "{record}");
    assert_eq!(record["checks"][0]["status"], "not_run", "{record}");
    assert_eq!(record["checks"][0]["exit"], 127, "{record}");
    let calls = tree_of(&record).parent().unwrap().join(".work-output/test/calls");
    assert!(!calls.exists(), "compile command started without its toolchain");
}

#[test]
fn a_declared_acceptance_crate_does_not_add_a_local_command() {
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
    assert_eq!(record["outcome"], "unverified", "{record}");
    assert_eq!(
        record["checks"],
        json!([
            {"origin": "derived", "command": CHECK_ENTRY, "exit": 0, "timed_out": false, "status": "passed"},
        ]),
        "{record}"
    );
    let calls = calls(tree_of(&record));
    assert_eq!(
        calls
            .lines()
            .map(|line| line.split('|').next().unwrap())
            .collect::<Vec<_>>(),
        ["check -p one"]
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
    assert_eq!(record["outcome"], "unverified", "{record}");
    // The repair prompt carries the derived failure and names the check that failed.
    assert_eq!(requests.len(), 3, "worker, then one repair over the same session");
    assert!(
        requests[2].messages.iter().any(|message| message
            .content
            .as_deref()
            .is_some_and(|text| text.contains("External acceptance checks failed")
                && text.contains("cargo check -p one"))),
        "repair prompt: {:?}",
        requests[2].messages.last().and_then(|m| m.content.as_deref())
    );
    let calls = calls(tree_of(&record));
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        ["check -p one", "check -p one"]
    );
    // The recorded evidence is the passing recheck, with nothing hidden.
    for check in record["checks"].as_array().unwrap() {
        assert_eq!(check["status"], "passed", "{record}");
    }
}

#[test]
fn a_compile_precheck_cannot_produce_a_saved_verification_receipt() {
    let env = TestEnv::new();
    setup_with_files(&env, "[]", &crate_files());
    let tools = tools_with_cargo(&env, RECORDING_CARGO);
    let path = path_with(&tools);
    let record = run_args(
        &env,
        vec![write_turn("crates/one/src/lib.rs"), ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        Some(path.clone()),
    );
    assert_eq!(record["outcome"], "unverified", "{record}");
    assert_eq!(record["checks"][0]["status"], "passed", "{record}");

    let rejected = verify(&env, Some(&path));
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(!rejected.status.success(), "{stderr}");
    assert!(
        stderr.contains("only finished successful work can be verified"),
        "{stderr}"
    );
    assert!(
        rejected.stdout.is_empty(),
        "a compile pass produced a verification receipt"
    );
}

#[test]
fn disabling_local_checks_leaves_a_changed_crate_unverified_without_receipts() {
    let env = TestEnv::new();
    setup_with_files(&env, "[]", &crate_files());
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"oxa":{"work":{"localChecks":[]}}}"#,
    );
    let tools = tools_with_cargo(&env, RECORDING_CARGO);
    let record = run_args(
        &env,
        vec![write_turn("crates/one/src/lib.rs"), ScriptTurn::text("finished")],
        &["work", "../tasks.json"],
        Some(path_with(&tools)),
    );
    assert_eq!(record["outcome"], "unverified", "{record}");
    assert!(record["checks"].as_array().is_none_or(Vec::is_empty), "{record}");
    let output = tree_of(&record).parent().unwrap().join(".work-output/test/calls");
    assert!(!output.exists(), "the runner started a disabled local check");
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
