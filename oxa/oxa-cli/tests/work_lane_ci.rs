//! Lane CI acceptance is the gate: a local pass alone is never done.
#[path = "common/work.rs"]
mod work;
use oxa_fake::{ScriptTurn, TestEnv};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, process::Command};
use work::{git, run_observed_with, setup};

fn state_path(env: &TestEnv) -> PathBuf {
    let common = env.work().join("project/.git").canonicalize().unwrap();
    let key: String = Sha256::digest(common.as_os_str().as_encoded_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    env.agent_dir().join("work").join(key).join("test.json")
}

fn configure(env: &TestEnv) -> String {
    use std::os::unix::fs::PermissionsExt;
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always","oxa":{"reportIssue":{"repo":"example/project"}}}"#,
    );
    let gh = env.write(
        "tmp/gh",
        r##"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/gh-calls"
case "$1 $2" in
  'repo view')
    printf '%s' '{"nameWithOwner":"example/project","url":"https://github.com/example/project"}' ;;
  'run list')
    head=$({ git rev-parse refs/heads/agent/test; })
    mode=failure
    if test -f "$HOME/lane-pass"; then mode=success; fi
    printf '[{"databaseId":7,"headSha":"%s","status":"completed","conclusion":"%s","event":"push"}]' "$head" "$mode" ;;
  'run view')
    for arg in "$@"; do
      if test "$arg" = '--log-failed'; then
        printf 'FAIL [   0.004s] oxa-work::lane::tests::synthetic_case\n'
        exit 0
      fi
    done
    printf '%s' '{"jobs":[{"name":"lane (oxa, ubuntu-24.04)","conclusion":"failure","steps":[{"name":"Affected crates","conclusion":"failure"}]}]}' ;;
  *) exit 7 ;;
esac
"##,
    );
    let git_wrapper = env.write(
        "tmp/git",
        "#!/bin/sh\nfor arg do if test \"$arg\" = push; then printf 'push\\n' >> \"$HOME/push-calls\"; fi; done\nexec /usr/bin/git \"$@\"\n",
    );
    let controller = env.write("tmp/oxc", "#!/bin/sh\nprintf '{}'\n");
    for file in [gh, git_wrapper, controller] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o755)).unwrap();
    }
    format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap())
}

fn lane_task(env: &TestEnv) {
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"agent/test","model":"fake","prompt":"Finish task","lane_ci":true}]"#,
    );
}

fn approve(command: &mut Command) {
    command.env("OXA_WORK_OWNER_CONFIRMED", "1");
}

fn run_lane(env: &TestEnv, turns: Vec<ScriptTurn>, path: String) -> serde_json::Value {
    run_observed_with(env, turns, &["work", "../tasks.json"], Some(path), approve).0
}

fn edits() -> Vec<ScriptTurn> {
    vec![
        ScriptTurn::tool("write", "write", r#"{"path":"answer.txt","content":"lane"}"#),
        ScriptTurn::text("done"),
    ]
}

fn repair_edits() -> Vec<ScriptTurn> {
    vec![
        ScriptTurn::tool("write", "write", r#"{"path":"answer.txt","content":"lane"}"#),
        ScriptTurn::text("done"),
        ScriptTurn::tool("write", "write", r#"{"path":"answer.txt","content":"repaired"}"#),
        ScriptTurn::text("repaired"),
    ]
}

fn origin_head(env: &TestEnv) -> String {
    git(
        env,
        &env.work().join("remote.git"),
        &["rev-parse", "refs/heads/agent/test"],
    )
}

fn command(env: &TestEnv, path: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(args)
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("PATH", path)
        .env("OXA_WORK_OWNER_CONFIRMED", "1")
        .output()
        .unwrap()
}

#[test]
fn a_green_lane_run_at_the_pushed_head_is_done() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    let record = run_lane(&env, edits(), path);
    assert_eq!(record["outcome"], "done", "{record}");
    assert_eq!(record["step"], "finished");
    assert_eq!(record["lane"]["conclusion"], "success", "{record}");
    assert_eq!(record["lane"]["head"], record["head_sha"], "{record}");
    assert_eq!(
        origin_head(&env),
        record["head_sha"].as_str().unwrap(),
        "the lane observed a different commit than the branch carries"
    );
    let push_calls = fs::read_to_string(env.home().join("push-calls")).unwrap();
    assert_eq!(push_calls.lines().count(), 1, "{push_calls}");
    let gh_calls = fs::read_to_string(env.home().join("gh-calls")).unwrap();
    assert!(
        gh_calls.lines().any(|line| line.starts_with("run list")),
        "{gh_calls}"
    );
    assert!(
        !gh_calls.lines().any(|line| line.starts_with("pr ")),
        "acceptance opened a pull request: {gh_calls}"
    );
    let state = fs::read_to_string(state_path(&env)).unwrap();
    assert!(state.contains("\"lane\""), "{state}");
}

#[test]
fn a_local_pass_with_a_failed_lane_run_is_red_after_one_repair() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    // No "lane-pass" marker: every lane observation fails.
    let record = run_lane(&env, repair_edits(), path);
    assert_ne!(record["outcome"], "done", "{record}");
    assert_eq!(record["outcome"], "red", "{record}");
    assert_eq!(record["lane_repairs"], 1, "{record}");
    assert_eq!(record["lane"]["conclusion"], "failure", "{record}");
    let detail = record["detail"].as_str().unwrap();
    assert!(
        detail.contains("oxa-work::lane::tests::synthetic_case"),
        "the repair was not told which test failed: {detail}"
    );
    assert_eq!(
        origin_head(&env),
        record["head_sha"].as_str().unwrap(),
        "the branch did not carry the repaired commit"
    );
    let push_calls = fs::read_to_string(env.home().join("push-calls")).unwrap();
    assert_eq!(push_calls.lines().count(), 2, "{push_calls}");
}

#[test]
fn publication_refuses_a_lane_task_without_green_evidence() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    env.write("home/lane-pass", "");
    let record = run_lane(&env, edits(), path.clone());
    assert_eq!(record["outcome"], "done", "{record}");
    // Remove the lane observation from the saved record: a saved local pass is
    // not publication evidence for a lane task.
    let path_to_state = state_path(&env);
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&path_to_state).unwrap()).unwrap();
    state["record"].as_object_mut().unwrap().remove("lane");
    fs::write(&path_to_state, serde_json::to_vec(&state).unwrap()).unwrap();
    let pushes_before = fs::read_to_string(env.home().join("push-calls"))
        .unwrap()
        .lines()
        .count();
    let out = command(&env, &path, &["work", "../tasks.json", "--publish"]);
    let published: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_eq!(published["outcome"], "blocked", "{published}");
    assert!(
        published["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("lane CI"),
        "{published}"
    );
    let pushes_after = fs::read_to_string(env.home().join("push-calls"))
        .unwrap()
        .lines()
        .count();
    assert_eq!(
        pushes_after, pushes_before,
        "publication pushed without lane evidence"
    );
    let gh_calls = fs::read_to_string(env.home().join("gh-calls")).unwrap();
    assert!(
        !gh_calls.lines().any(|line| line.starts_with("pr create")),
        "{gh_calls}"
    );
}

#[test]
fn a_lane_task_without_owner_push_approval_runs_its_local_checks_but_is_never_done() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    lane_task(&env);
    let path = configure(&env);
    let server = oxa_fake::FakeServer::start(edits(), oxa_fake::FakeOpts::default()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .env("PATH", path)
        .env("OXA_CODING_AGENT", "1")
        .output()
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_ne!(record["outcome"], "done", "{record}");
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(
        record["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("push confirmation"),
        "{record}"
    );
    assert!(
        server.requests().is_empty(),
        "an unapproved lane task started a worker"
    );
    assert!(
        !env.home().join("push-calls").exists(),
        "an unapproved lane task pushed"
    );
}

#[test]
fn a_lane_task_only_accepts_lane_triggering_branches() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    env.write(
        "work/tasks.json",
        r#"[{"id":"test","branch":"feature/test","model":"fake","prompt":"Finish task","lane_ci":true}]"#,
    );
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("OXA_CODING_AGENT", "1")
        .output()
        .unwrap();
    // An invalid lane task is refused while reading the task file, before any
    // worktree or worker exists.
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("lane workflow runs on"), "{stderr}");
    assert!(!env.work().join(".oxa-worktrees").exists());
}
