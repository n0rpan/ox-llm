//! Local bare pushes and fake gh, including draft updates and merge-queue protection.
#[path = "common/work.rs"]
mod work;
use oxa_fake::{ScriptTurn, TestEnv};
use std::{fs, path::PathBuf, process::Command};
use work::{git, run, run_args, run_observed, setup};
fn configure(env: &TestEnv) -> String {
    use std::os::unix::fs::PermissionsExt;
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always","oxa":{"reportIssue":{"repo":"example/project"}}}"#,
    );
    let gh = env.write("tmp/gh", r##"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/gh-calls"
case "$1 $2" in
  'repo view') printf '%s' '{"nameWithOwner":"example/project"}' ;;
  'pr list') if test -f "$HOME/pr-state.json"; then printf '['; cat "$HOME/pr-state.json"; printf ']'; else printf '[]'; fi ;;
  'api graphql') if test -f "$HOME/queued"; then printf '%s' '{"data":{"repository":{"pullRequest":{"mergeQueueEntry":{"id":"queue"}}}}}'; else printf '%s' '{"data":{"repository":{"pullRequest":{"mergeQueueEntry":null}}}}'; fi ;;
  'pr create')
    draft=false
    body_next=false
    for arg in "$@"; do
      if test "$body_next" = true; then cat "$arg" > "$HOME/published-body"; body_next=false; fi
      if test "$arg" = '--draft'; then draft=true; fi
      if test "$arg" = '--body-file'; then body_next=true; fi
    done
    printf '{"number":1,"url":"https://github.com/example/project/pull/1","isDraft":%s}' "$draft" > "$HOME/pr-state.json"
    printf '%s' 'https://github.com/example/project/pull/1' ;;
  'pr edit') : ;;
  'pr ready')
    draft=false
    for arg in "$@"; do if test "$arg" = '--undo'; then draft=true; fi; done
    printf '{"number":1,"url":"https://github.com/example/project/pull/1","isDraft":%s}' "$draft" > "$HOME/pr-state.json" ;;
  *) exit 7 ;;
esac
"##);
    let git_wrapper = env.write("tmp/git", "#!/bin/sh\nif test \"$1\" = push; then printf 'push\\n' >> \"$HOME/push-calls\"; fi\nexec /usr/bin/git \"$@\"\n");
    let controller = env.write("tmp/oxc", "#!/bin/sh\nprintf '{}'\n");
    for file in [gh, git_wrapper, controller] {
        fs::set_permissions(file, fs::Permissions::from_mode(0o755)).unwrap();
    }
    format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap())
}
fn edits() -> Vec<ScriptTurn> {
    vec![
        ScriptTurn::tool("write", "write", r#"{"path":"answer.txt","content":"published"}"#),
        ScriptTurn::text("done"),
    ]
}
#[test]
fn publishes_local_branch_and_updates_same_pr_on_restart() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let path = configure(&env);
    let first = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(path.clone()),
    );
    assert_eq!(first["outcome"], "done", "{first}");
    assert_eq!(first["draft"], false);
    assert_eq!(first["pull_request"], "https://github.com/example/project/pull/1");
    assert_eq!(
        git(
            &env,
            &env.work(),
            &[
                "--git-dir",
                "remote.git",
                "show",
                "refs/heads/agent/test:answer.txt"
            ]
        ),
        "published"
    );
    let again = run_args(&env, vec![], &["work", "../tasks.json", "--publish"], Some(path));
    assert_eq!(again["outcome"], "done", "{again}");
    let calls = fs::read_to_string(env.home().join("gh-calls")).unwrap();
    assert_eq!(calls.lines().filter(|l| l.starts_with("pr create")).count(), 1);
    assert_eq!(calls.lines().filter(|l| l.starts_with("pr edit")).count(), 1);
    let body = fs::read_to_string(env.home().join("published-body")).unwrap();
    assert!(!body.contains("Finish task"));
}
#[test]
fn red_checks_create_a_draft_pr() {
    let env = TestEnv::new();
    setup(&env, r#"["printf red > failed-check; exit 1"]"#);
    let path = configure(&env);
    let record = run_args(
        &env,
        vec![ScriptTurn::text("done"), ScriptTurn::text("still red")],
        &["work", "../tasks.json", "--publish"],
        Some(path),
    );
    assert_eq!(record["outcome"], "red", "{record}");
    assert_eq!(record["draft"], true);
    assert!(record["pull_request"].is_string());
}
#[test]
fn queued_pr_is_never_pushed_and_publication_retries_without_agent_replay() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let path = configure(&env);
    run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(path.clone()),
    );
    env.write("home/queued", "yes");
    let blocked = run_args(
        &env,
        vec![],
        &["work", "../tasks.json", "--publish"],
        Some(path.clone()),
    );
    assert_eq!(blocked["outcome"], "blocked", "{blocked}");
    assert!(blocked["detail"].as_str().unwrap().contains("merge queue"));
    assert_eq!(
        fs::read_to_string(env.home().join("push-calls"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    fs::remove_file(env.home().join("queued")).unwrap();
    let retried = run_args(&env, vec![], &["work", "../tasks.json", "--publish"], Some(path));
    assert_eq!(retried["outcome"], "done", "{retried}");
}
#[test]
fn offline_publication_never_starts_gh_or_push() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let path = configure(&env);
    let record = run_args(
        &env,
        edits(),
        &["--offline", "work", "../tasks.json", "--publish"],
        Some(path),
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("offline"));
    assert!(!env.home().join("gh-calls").exists());
    assert!(!env.home().join("push-calls").exists());
}
#[test]
fn branch_drift_after_acceptance_cannot_publish_unchecked_commits() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let path = configure(&env);
    let done = run(&env, edits());
    let tree = PathBuf::from(done["worktree"].as_str().unwrap());
    fs::write(tree.join("later.txt"), "unchecked").unwrap();
    git(&env, &tree, &["add", "."]);
    git(&env, &tree, &["commit", "-m", "later commit"]);
    let record = run_args(&env, vec![], &["work", "../tasks.json", "--publish"], Some(path));
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"]
        .as_str()
        .unwrap()
        .contains("changed after acceptance"));
    assert!(!env.home().join("push-calls").exists());
}
#[test]
fn own_problem_notes_are_captured_and_failed_search_keeps_a_local_draft() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let path = configure(&env);
    let text = "done <oxa-work-reports>[{\"title\":\"oxa work symptom\",\"notes\":\"Synthetic reproduction token=example\",\"keywords\":[\"oxa work\",\"synthetic symptom\"]}]</oxa-work-reports>";
    let record = run_args(
        &env,
        vec![ScriptTurn::text(text)],
        &["work", "../tasks.json", "--report-problems"],
        Some(path),
    );
    assert_eq!(record["problem_notes"].as_array().unwrap().len(), 1);
    assert_eq!(record["reports"][0]["disposition"], "local");
    let draft = fs::read_to_string(record["reports"][0]["draft"].as_str().unwrap()).unwrap();
    assert!(!draft.contains("token=example"));
    let calls = fs::read_to_string(env.home().join("gh-calls")).unwrap();
    assert!(!calls.contains("issue create"));
}
#[test]
fn a_model_tool_cannot_opt_itself_into_reporting_or_publishing() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let path = configure(&env);
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json", "--report-problems"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("OXA_CODING_AGENT", "1")
        .env("PATH", path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(!env.home().join("gh-calls").exists());
}

#[test]
fn green_rerun_marks_an_existing_red_draft_ready() {
    let env = TestEnv::new();
    setup(
        &env,
        r#"["test -f fixed || { printf red > failed-check; exit 1; }"]"#,
    );
    let path = configure(&env);
    let red = run_args(
        &env,
        vec![ScriptTurn::text("done"), ScriptTurn::text("still red")],
        &["work", "../tasks.json", "--publish"],
        Some(path.clone()),
    );
    assert_eq!(red["draft"], true, "{red}");
    let green = run_args(
        &env,
        vec![
            ScriptTurn::tool("fix", "write", r#"{"path":"fixed","content":"fixed"}"#),
            ScriptTurn::text("done"),
        ],
        &["work", "../tasks.json", "--rerun", "--publish"],
        Some(path),
    );
    assert_eq!(green["outcome"], "done", "{green}");
    assert_eq!(green["draft"], false);
    assert!(fs::read_to_string(env.home().join("gh-calls"))
        .unwrap()
        .contains("pr ready"));
}
#[test]
fn a_local_report_retries_without_replaying_the_completed_task() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let path = configure(&env);
    let text = "<oxa-work-reports>[{\"title\":\"oxa work symptom\",\"notes\":\"Synthetic reproduction\",\"keywords\":[\"oxa work\",\"synthetic symptom\"]}]</oxa-work-reports>";
    let first = run_args(
        &env,
        vec![ScriptTurn::text(text)],
        &["work", "../tasks.json", "--report-problems"],
        Some(path.clone()),
    );
    assert_eq!(first["reports"][0]["disposition"], "local");
    let gh = env.tmp().join("gh");
    let script = fs::read_to_string(&gh).unwrap().replace("  *) exit 7 ;;", "  'issue list') printf '[]' ;;\n  'issue create') printf '%s' 'https://github.com/example/project/issues/2' ;;\n  *) exit 7 ;;");
    fs::write(gh, script).unwrap();
    let retry = run_args(
        &env,
        vec![],
        &["work", "../tasks.json", "--report-problems"],
        Some(path),
    );
    assert_eq!(retry["reports"][0]["disposition"], "filed", "{retry}");
    assert_eq!(retry["step"], "finished");
}

#[test]
fn unfinished_execution_keeps_its_saved_offline_restriction() {
    use oxa_fake::{FakeModel, FakeOpts, FakeServer};
    let env = TestEnv::new();
    setup(&env, "[]");
    run_args(
        &env,
        vec![ScriptTurn::text("done")],
        &["--offline", "work", "../tasks.json"],
        None,
    );
    let state_path = env.work().join("project/.git/oxa-work/test.json");
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["step"] = "agent".into();
    state["outcome"] = "running".into();
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let server = FakeServer::start(
        vec![ScriptTurn::text("network was allowed")],
        FakeOpts {
            remote: Some("openrouter"),
            models: vec![FakeModel {
                alias: "fake-model".into(),
                cap: 64000,
                max_output: Some(4096),
                caps: None,
            }],
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "../tasks.json"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs(&server.url()))
        .output()
        .unwrap();
    let record: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(record["detail"].as_str().unwrap().contains("offline"));
    assert!(server.requests().is_empty());
}
#[test]
fn later_publication_and_reporting_do_not_erase_execution_policy() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let path = configure(&env);
    run_args(
        &env,
        edits(),
        &["--offline", "work", "../tasks.json"],
        Some(path.clone()),
    );
    let state_path = env.work().join("project/.git/oxa-work/test.json");
    for flag in ["--publish", "--report-problems"] {
        let record = run_args(&env, vec![], &["work", "../tasks.json", flag], Some(path.clone()));
        assert_eq!(record["outcome"], "done", "{record}");
        let state: serde_json::Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(state["offline"], true, "{flag} erased execution policy");
    }
}
#[test]
fn optional_publication_metadata_defaults_to_inactive() {
    let env = TestEnv::new();
    setup(&env, "[]");
    run(&env, vec![ScriptTurn::text("done")]);
    let state_path = env.work().join("project/.git/oxa-work/test.json");
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    for key in [
        "head",
        "pull_request",
        "draft",
        "problem_notes",
        "reports",
        "acceptance_outcome",
    ] {
        state.as_object_mut().unwrap().remove(key);
    }
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let record = run(&env, vec![]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert_eq!(record["draft"], false);
    assert_eq!(record["reports"], serde_json::json!([]));
}
#[test]
fn failed_draft_creation_does_not_strand_reporting_checkpoint() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let path = configure(&env);
    env.write(
        "home/.oxa/agent/issue-drafts",
        "a file blocks the draft directory",
    );
    let text="<oxa-work-reports>[{\"title\":\"oxa work symptom\",\"notes\":\"Synthetic reproduction\",\"keywords\":[\"oxa work\",\"synthetic symptom\"]}]</oxa-work-reports>";
    let first = run_args(
        &env,
        vec![ScriptTurn::text(text)],
        &["work", "../tasks.json", "--report-problems"],
        Some(path.clone()),
    );
    assert_eq!(first["step"], "finished", "{first}");
    assert_eq!(first["reports"][0]["disposition"], "failed");
    let plain = run_args(&env, vec![], &["work", "../tasks.json"], Some(path.clone()));
    assert_eq!(plain["step"], "finished");
    fs::remove_file(env.home().join(".oxa/agent/issue-drafts")).unwrap();
    let retry = run_args(
        &env,
        vec![],
        &["work", "../tasks.json", "--report-problems"],
        Some(path),
    );
    assert_eq!(retry["reports"][0]["disposition"], "local");
    assert_eq!(retry["step"], "finished");
}

#[test]
fn unsetting_the_agent_marker_and_piping_yes_cannot_authorize_public_writes() {
    use std::io::Write;
    use std::process::Stdio;
    let mut unauthorized = vec![];
    for flag in ["--publish", "--report-problems"] {
        let env = TestEnv::new();
        setup(&env, "[]");
        let path = configure(&env);
        env.write("tmp/gh", r##"#!/bin/sh
case "$1 $2" in
  'repo view') printf '%s' '{"nameWithOwner":"example/project"}' ;;
  'pr list'|'issue list') printf '[]' ;;
  'pr create'|'issue create'|'issue comment') touch "$HOME/public-write"; printf '%s' 'https://github.com/example/project/issues/1' ;;
  *) exit 7 ;;
esac
"##);
        let text = "<oxa-work-reports>[{\"title\":\"oxa work symptom\",\"notes\":\"Synthetic reproduction\",\"keywords\":[\"oxa work\",\"synthetic symptom\"]}]</oxa-work-reports>";
        let mut turns = edits();
        turns.pop();
        turns.push(ScriptTurn::text(text));
        assert_eq!(run(&env, turns)["outcome"], "done");
        // A fresh session has no controlling terminal. Removing the marker and
        // supplying yes on stdin must not stand in for a person's confirmation.
        let mut child = Command::new("setsid")
            .env_clear()
            .envs(env.envs("http://127.0.0.1:1"))
            .env("PATH", path)
            .env_remove("OXA_CODING_AGENT")
            .args(["--wait", env!("CARGO_BIN_EXE_oxa"), "work", "../tasks.json", flag])
            .current_dir(env.work().join("project"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"yes\n").unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.code().is_some());
        if env.home().join("public-write").exists() || env.home().join("push-calls").exists() {
            unauthorized.push(flag);
        }
    }
    assert!(
        unauthorized.is_empty(),
        "public writes without confirmation: {unauthorized:?}"
    );
}

#[test]
fn rerun_resumes_interrupted_execution_without_discarding_its_checkpoint() {
    for step in ["acceptance", "recheck", "saving"] {
        let env = TestEnv::new();
        setup(&env, r#"["printf checked >> checkpoint-checks"]"#);
        let first = run(&env, edits());
        let state_path = env.work().join("project/.git/oxa-work/test.json");
        let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        state["step"] = step.into();
        state["outcome"] = if step == "saving" { "done" } else { "running" }.into();
        state["pull_request"] = "https://github.com/example/project/pull/1".into();
        state["problem_notes"] = serde_json::json!([{"title":"Synthetic problem","notes":"Keep these notes","keywords":["synthetic problem","keep notes"]}]);
        state["reports"] = serde_json::json!([{"draft":"report.md","disposition":"existing_issue","url":"https://github.com/example/project/issues/1","detail":""}]);
        fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        let (record, requests) = run_observed(&env, vec![], &["work", "../tasks.json", "--rerun"], None);
        assert_eq!(record["outcome"], "done", "{step}: {record}");
        assert!(requests.is_empty(), "{step}: agent was replayed");
        for key in ["pull_request", "problem_notes", "reports"] {
            assert_eq!(record[key], state[key], "{step}: {key}");
        }
        let after: serde_json::Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(after["checkpoint"], state["checkpoint"], "{step}");
        assert_eq!(record["worktree"], first["worktree"]);
    }
}
