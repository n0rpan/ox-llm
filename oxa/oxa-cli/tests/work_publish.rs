//! Local bare pushes and fake gh, including draft updates and merge-queue protection.
#[path = "common/work.rs"]
mod work;
use oxa_fake::{ScriptTurn, TestEnv};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, process::Command};
use work::{git, run, run_args, setup};
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
    let gh = env.write("tmp/gh", r##"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/gh-calls"
host=github.com
if test -f "$HOME/enterprise-host"; then host=code.example.invalid; fi
case "$1 $2" in
  'repo view')
    if test "${GH_ENTERPRISE_TOKEN+x}" = x || test "${GITHUB_ENTERPRISE_TOKEN+x}" = x; then
      touch "$HOME/ambient-publication-auth"
    fi
    printf '{"nameWithOwner":"example/project","url":"https://%s/example/project"}' "$host"
    if test -f "$HOME/oversized-repo-response"; then head -c 1048576 /dev/zero | tr '\000' ' '; fi ;;
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
    printf '{"number":1,"url":"https://%s/example/project/pull/1","isDraft":%s,"headRepositoryOwner":{"login":"example"},"isCrossRepository":false}' "$host" "$draft" > "$HOME/pr-state.json"
    printf 'https://%s/example/project/pull/1' "$host" ;;
  'pr view') jq -Rs '{body:.}' "$HOME/published-body" ;;
  'pr edit')
    body_next=false
    for arg in "$@"; do
      if test "$body_next" = true; then cat "$arg" > "$HOME/published-body"; body_next=false; fi
      if test "$arg" = '--body-file'; then body_next=true; fi
    done ;;
  'pr ready')
    draft=false
    for arg in "$@"; do if test "$arg" = '--undo'; then draft=true; fi; done
    printf '{"number":1,"url":"https://%s/example/project/pull/1","isDraft":%s,"headRepositoryOwner":{"login":"example"},"isCrossRepository":false}' "$host" "$draft" > "$HOME/pr-state.json" ;;
  *) exit 7 ;;
esac
"##);
    let git_wrapper = env.write("tmp/git", "#!/bin/sh\nfor arg do if test \"$arg\" = push; then printf 'push\\n' >> \"$HOME/push-calls\"; fi; done\nexec /usr/bin/git \"$@\"\n");
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
fn a_real_save_failure_retries_without_another_worker() {
    exercise_save_recovery(
        "a_real_save_failure_retries_without_another_worker",
        SaveRecovery::Unchanged,
    );
}

#[test]
fn publication_does_not_replace_a_pending_save_checkpoint() {
    exercise_save_recovery(
        "publication_does_not_replace_a_pending_save_checkpoint",
        SaveRecovery::PendingPublish,
    );
}

#[test]
fn an_edit_after_acceptance_cannot_reuse_the_pending_save_verdict() {
    exercise_save_recovery(
        "an_edit_after_acceptance_cannot_reuse_the_pending_save_verdict",
        SaveRecovery::Edited,
    );
}

#[derive(PartialEq, Eq)]
enum SaveRecovery {
    Unchanged,
    PendingPublish,
    Edited,
}

fn exercise_save_recovery(test_name: &str, case: SaveRecovery) {
    const CHILD: &str = "OXA_SAVE_RECOVERY_TEST_ROOT";
    if let Some(root) = std::env::var_os(CHILD) {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Mutex,
        };
        let root = PathBuf::from(root);
        let project = root.join("work/project");
        let task = oxa_work::read_tasks(&root.join("work/tasks.json"))
            .unwrap()
            .remove(0);
        let policy = oxa_work::RunPolicy {
            git_binary: "/usr/bin/git".into(),
            host_path: std::env::var_os("PATH").unwrap(),
            terminate_binary: Some("/usr/bin/kill".into()),
            runner_owns_github: false,
            offline: false,
            storage: root.join("home/.oxa/agent/work/save-recovery"),
        };
        let binary = std::path::Path::new(env!("CARGO_BIN_EXE_oxa"));
        let injected = AtomicBool::new(false);
        let lock_path = Mutex::new(None);
        let observe = |record: &oxa_work::Record| {
            if record.step == oxa_work::Step::Saving && !injected.swap(true, Ordering::SeqCst) {
                let out = Command::new("/usr/bin/git")
                    .current_dir(&record.worktree)
                    .args(["rev-parse", "--path-format=absolute", "--git-path", "index"])
                    .output()
                    .map_err(|error| error.to_string())?;
                if !out.status.success() {
                    return Err("cannot locate synthetic index".into());
                }
                let path = PathBuf::from(format!("{}.lock", String::from_utf8_lossy(&out.stdout).trim()));
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(|error| error.to_string())?;
                *lock_path.lock().unwrap() = Some(path);
            }
            Ok(())
        };
        let plan = oxa_work::plan_task(&project, task.clone(), &policy).unwrap();
        let first =
            oxa_work::run_task_with_observer(&project, binary, &plan, &policy, false, &observe).unwrap();
        assert_eq!(first.outcome, oxa_work::Outcome::Blocked, "{first:?}");
        assert_eq!(first.step, oxa_work::Step::Saving, "{first:?}");
        assert!(matches!(
            first.worker.as_ref().unwrap().outcome,
            oxa_work::WorkerOutcome::Done
        ));
        let reported = oxa_work::report_notes(
            &task,
            &policy,
            &|_: &oxa_work::Record| Ok(()),
            &mut |_, _, _, _| panic!("no notes were emitted by this worker"),
        )
        .unwrap();
        assert_eq!(reported.step, oxa_work::Step::Saving, "{reported:?}");
        assert_eq!(reported.session_id, first.session_id);
        if case == SaveRecovery::PendingPublish {
            let published = oxa_work::publish(
                &project,
                binary,
                &task,
                &policy,
                std::path::Path::new("/unavailable/gh"),
                &|_: &oxa_work::Record| Ok(()),
                &|_| None,
                &mut |_| panic!("a pending save cannot request publication approval"),
            )
            .unwrap();
            assert_eq!(published.step, oxa_work::Step::Saving, "{published:?}");
            assert_eq!(published.checkpoint_revision, reported.checkpoint_revision);
        }
        fs::remove_file(lock_path.into_inner().unwrap().unwrap()).unwrap();
        if case == SaveRecovery::Edited {
            fs::write(first.worktree.join("answer.txt"), "edited after acceptance").unwrap();
        }
        let plan = oxa_work::plan_task_for_run(&project, task, &policy, false).unwrap();
        let retry = oxa_work::run_task_with_policy(&project, binary, &plan, &policy).unwrap();
        if case == SaveRecovery::Edited {
            assert!(
                matches!(retry.outcome, oxa_work::Outcome::Red | oxa_work::Outcome::Blocked),
                "{retry:?}"
            );
            assert_eq!(
                fs::read_to_string(first.worktree.join("answer.txt")).unwrap(),
                "edited after acceptance"
            );
            return;
        }
        assert_eq!(retry.outcome, oxa_work::Outcome::Done, "{retry:?}");
        assert_eq!(retry.step, oxa_work::Step::Finished);
        assert_eq!(retry.session_id, first.session_id);
        let saved = Command::new("/usr/bin/git")
            .current_dir(&retry.worktree)
            .args(["show", "HEAD:answer.txt"])
            .output()
            .unwrap();
        assert!(saved.status.success());
        assert_eq!(String::from_utf8(saved.stdout).unwrap(), "published");
        return;
    }
    let env = TestEnv::new();
    let checks = if case == SaveRecovery::Edited {
        r#"["grep -Fx published answer.txt"]"#
    } else {
        r#"["test -f .oxa/settings.json"]"#
    };
    setup(&env, checks);
    let server = oxa_fake::FakeServer::start(edits(), oxa_fake::FakeOpts::default()).unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name])
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
    assert_eq!(server.requests().len(), 2, "save retry must not call a model");
}

#[test]
fn a_legacy_done_without_checks_cannot_publish() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let record = run(&env, edits());
    assert_eq!(record["outcome"], "unverified", "{record}");
    let state_path = state_path(&env);
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["record"]["outcome"] = "done".into();
    fs::write(state_path, serde_json::to_vec(&state).unwrap()).unwrap();

    let (records, requests, code) = work::run_with_exit(
        &env,
        vec![],
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_eq!(code, 1, "{records:?}");
    assert_eq!(records[0]["outcome"], "blocked", "{records:?}");
    assert!(records[0]["detail"]
        .as_str()
        .unwrap()
        .contains("no acceptance checks"));
    assert!(requests.is_empty());
    assert!(!env.home().join("push-calls").exists());
    assert!(!env.home().join("gh-calls").exists());
}

#[test]
fn publishing_requires_acceptance_without_ignored_helpers() {
    let env = TestEnv::new();
    setup(&env, r#"["sh helper.sh"]"#);
    let repo = env.work().join("project");
    env.write("work/project/.gitignore", "helper.sh\n");
    git(&env, &repo, &["add", ".gitignore"]);
    git(&env, &repo, &["commit", "-m", "Ignore synthetic local helper"]);
    let mut turns = edits();
    turns.insert(
        0,
        ScriptTurn::tool("helper", "write", r#"{"path":"helper.sh","content":"exit 0\n"}"#),
    );
    let saved = run_args(
        &env,
        turns,
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_eq!(saved["outcome"], "blocked", "{saved}");
    assert!(saved["detail"]
        .as_str()
        .unwrap()
        .contains("clean committed acceptance"));
    assert!(!env.home().join("push-calls").exists());
    assert!(!env.home().join("gh-calls").exists());
}
#[test]
fn publication_refuses_oversized_repository_metadata_before_push() {
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    env.write("home/oversized-repo-response", "");
    let record = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    assert!(env.home().join("gh-calls").exists());
    assert!(!env.home().join("push-calls").exists());
}

#[test]
fn publication_does_not_forward_ambient_enterprise_credentials_to_origin() {
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    env.write("home/enterprise-host", "");
    let (record, _) = work::run_observed_with(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
        |command| {
            command.env("GH_ENTERPRISE_TOKEN", "synthetic-fixture");
            command.env("GITHUB_ENTERPRISE_TOKEN", "synthetic-fixture");
        },
    );
    assert_eq!(record["outcome"], "done", "{record}");
    assert!(env.home().join("push-calls").exists());
    assert!(!env.home().join("ambient-publication-auth").exists());
}

#[test]
fn publication_refuses_a_same_owner_fork_before_push() {
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    env.write(
        "home/pr-state.json",
        r#"{"number":1,"url":"https://github.com/example/project/pull/1","isDraft":false,"headRepositoryOwner":{"login":"example"},"isCrossRepository":true}"#,
    );
    env.write("home/published-body", "Synthetic fork");
    let record = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_eq!(record["outcome"], "blocked", "{record}");
    let calls = fs::read_to_string(env.home().join("gh-calls")).unwrap();
    assert!(calls.contains("pr list"));
    assert!(!env.home().join("push-calls").exists());
}

#[test]
fn publishes_local_branch_and_updates_same_pr_on_restart() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    let path = configure(&env);
    let hook = env.write(
        "work/project/.git/hooks/pre-push",
        "#!/bin/sh\ntouch \"$HOME/publish-hook-ran\"\n",
    );
    fs::set_permissions(hook, fs::Permissions::from_mode(0o755)).unwrap();
    let first = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(path.clone()),
    );
    assert_eq!(first["outcome"], "done", "{first}");
    assert!(!env.home().join("publish-hook-ran").exists());
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
fn publication_keeps_the_origin_host_for_prs_and_graphql() {
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    let path = configure(&env);
    env.write("home/enterprise-host", "synthetic");
    let first = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(path.clone()),
    );
    assert_eq!(first["outcome"], "done", "{first}");
    let again = run_args(&env, vec![], &["work", "../tasks.json", "--publish"], Some(path));
    assert_eq!(again["outcome"], "done", "{again}");
    let calls = fs::read_to_string(env.home().join("gh-calls")).unwrap();
    assert!(
        calls.contains("pr list --repo code.example.invalid/example/project"),
        "{calls}"
    );
    assert!(
        calls.contains("pr create --repo code.example.invalid/example/project"),
        "{calls}"
    );
    assert!(
        calls.contains("api graphql --hostname code.example.invalid"),
        "{calls}"
    );
}

#[test]
fn rerun_updates_managed_metadata_without_erasing_owner_body() {
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    let path = configure(&env);
    let first = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(path.clone()),
    );
    assert_eq!(first["outcome"], "done", "{first}");
    let body_path = env.home().join("published-body");
    let body =
        fs::read_to_string(&body_path).unwrap() + "\nOwner what/why and validated controls stay here.\n";
    fs::write(&body_path, body).unwrap();
    let next = run_args(
        &env,
        vec![
            ScriptTurn::tool("update", "write", r#"{"path":"answer.txt","content":"updated"}"#),
            ScriptTurn::text("done"),
        ],
        &["work", "../tasks.json", "--rerun", "--publish"],
        Some(path),
    );
    assert_eq!(next["outcome"], "done", "{next}");
    assert_ne!(next["head_sha"], first["head_sha"]);
    let body = fs::read_to_string(body_path).unwrap();
    assert!(body.contains("Owner what/why and validated controls stay here."));
    assert!(body.contains(next["head_sha"].as_str().unwrap()), "{body}");
    assert!(!body.contains(first["head_sha"].as_str().unwrap()), "{body}");
}
#[test]
fn red_checks_never_push_or_create_a_pr() {
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
    assert_eq!(record["draft"], false);
    assert!(record["pull_request"].is_null());
    assert!(!env.home().join("gh-calls").exists());
    assert!(!env.home().join("push-calls").exists());
}
#[test]
fn queued_pr_is_never_pushed_and_publication_retries_without_agent_replay() {
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
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
    assert!(
        blocked["detail"].as_str().unwrap().contains("merge queue"),
        "{blocked}"
    );
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
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
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
fn publication_drift_requires_fresh_checks_on_explicit_rerun() {
    let env = TestEnv::new();
    setup(&env, r#"["test ! -f later.txt"]"#);
    let path = configure(&env);
    let done = run(&env, edits());
    let tree = PathBuf::from(done["worktree"].as_str().unwrap());
    fs::write(tree.join("later.txt"), "unchecked\n").unwrap();
    git(&env, &tree, &["add", "."]);
    git(
        &env,
        &tree,
        &["commit", "-m", "Synthetic acceptance-breaking drift"],
    );
    let blocked = run_args(
        &env,
        vec![],
        &["work", "../tasks.json", "--publish"],
        Some(path.clone()),
    );
    assert_eq!(blocked["step"], "publishing", "{blocked}");
    let (retry, requests) = work::run_observed(
        &env,
        vec![ScriptTurn::text("done"), ScriptTurn::text("done")],
        &["work", "../tasks.json", "--rerun", "--publish"],
        Some(path),
    );
    assert_eq!(retry["outcome"], "red", "{retry}");
    assert_eq!(
        requests.len(),
        2,
        "fresh execution plus the existing repair attempt"
    );
    assert!(!env.home().join("push-calls").exists());
}

#[test]
fn ordinary_run_cannot_reseal_a_failed_publication_checkpoint() {
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    let path = configure(&env);
    let done = run(&env, edits());
    let tree = PathBuf::from(done["worktree"].as_str().unwrap());
    fs::write(tree.join("later.txt"), "unchecked\n").unwrap();
    git(&env, &tree, &["add", "."]);
    git(&env, &tree, &["commit", "-m", "Synthetic publication drift"]);
    let blocked = run_args(
        &env,
        vec![],
        &["work", "../tasks.json", "--publish"],
        Some(path.clone()),
    );
    assert_eq!(blocked["step"], "publishing", "{blocked}");
    let (retry, requests) = work::run_observed(&env, vec![], &["work", "../tasks.json"], Some(path.clone()));
    assert_eq!(
        retry["head_sha"], done["head_sha"],
        "a normal run cannot certify a new head"
    );
    assert!(requests.is_empty());
    let retry = run_args(&env, vec![], &["work", "../tasks.json", "--publish"], Some(path));
    assert_eq!(retry["outcome"], "blocked", "{retry}");
    assert!(!env.home().join("push-calls").exists());
}

#[test]
fn branch_drift_after_acceptance_cannot_publish_unchecked_commits() {
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
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
    assert!(
        !serde_json::to_string(&record).unwrap().contains("token=example"),
        "unscrubbed record output"
    );
    assert!(
        !fs::read_to_string(state_path(&env))
            .unwrap()
            .contains("token=example"),
        "unscrubbed checkpoint"
    );
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
fn green_rerun_can_publish_after_red_remained_local() {
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
    assert_eq!(red["draft"], false, "{red}");
    assert!(red["pull_request"].is_null());
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
        .contains("pr create"));
}
#[test]
fn semantic_work_report_without_evidence_stays_local() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let path = configure(&env);
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always","oxa":{"reportIssue":{"repo":"example/project","semanticDedupe":true}}}"#,
    );
    let gh = env.tmp().join("gh");
    let script = fs::read_to_string(&gh).unwrap().replace(
        "  *) exit 7 ;;",
        "  'issue list') printf '[]' ;;\n  'issue create') printf '%s' 'https://github.com/example/project/issues/2' ;;\n  *) exit 7 ;;",
    );
    fs::write(gh, script).unwrap();
    let text = concat!(
        "<oxa-work-reports>",
        r#"[{"title":"oxa work symptom","notes":"Synthetic reproduction","keywords":["oxa work","synthetic symptom"]}]"#,
        "</oxa-work-reports>",
    );
    let record = run_args(
        &env,
        vec![ScriptTurn::text(text)],
        &["work", "../tasks.json", "--report-problems"],
        Some(path),
    );
    let report = &record["reports"][0];
    assert_eq!(report["disposition"], "local", "{record}");
    assert!(std::path::Path::new(report["draft"].as_str().unwrap()).is_file());
    assert!(report["url"].is_null());
    assert!(report["detail"]
        .as_str()
        .unwrap()
        .contains("Observed and Evidence"));
    assert!(!env.home().join("gh-calls").exists());
}

#[test]
fn semantic_work_reports_file_once_and_comment_on_four_paraphrases() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let path = configure(&env);
    env.write(
        "home/.oxa/agent/settings.json",
        r#"{"defaultProjectTrust":"always","oxa":{"reportIssue":{"repo":"example/project","semanticDedupe":true}}}"#,
    );
    let gh = env.tmp().join("gh");
    let script = fs::read_to_string(&gh).unwrap().replace(
        "  *) exit 7 ;;",
        r##"  'api repos/example/project') printf 'public' ;;
  'api repos/example/project/git/ref/heads/main')
    printf '1234567890abcdef1234567890abcdef12345678' ;;
  'api -H') printf 'line 1\nline 2\nline 3\nline 4\nreturn an unexpected default\n' ;;
  'search issues') printf '[]' ;;
  'issue view')
    printf '%s' '{"number":2,"title":"Original symptom","body":"Original failure","labels":[]}' ;;
  'issue create')
    for fd in /proc/self/fd/*; do
      if test "$fd" -ef "$HOME/.oxa/agent/work-report.lock"; then
        touch "$HOME/report-lease-inherited"
      fi
    done
    printf '%s' 'https://github.com/example/project/issues/2' ;;
  'issue comment')
    printf '%s' 'https://github.com/example/project/issues/2#issuecomment-3' ;;
  *) exit 7 ;;"##,
    );
    fs::write(gh, script).unwrap();
    let summaries = [
        "The worker returned the default",
        "The result fell back without warning",
        "Task produced an unexpected default",
        "The worker ignored the custom result",
        "Processing emitted the placeholder",
    ];
    let notes: Vec<_> = summaries
        .iter()
        .map(|summary| {
            serde_json::json!({
                "title": summary,
                "notes": format!("Observed: oxa work: {summary}\nEvidence: src/worker.rs:5"),
                "keywords": ["worker result", "unexpected fallback"]
            })
        })
        .collect();
    let mut turns = vec![
        ScriptTurn::text(&format!(
            "<oxa-work-reports>{}</oxa-work-reports>",
            serde_json::json!(notes)
        )),
        ScriptTurn::text(r#"{"verified":true,"match":null,"rejected":[]}"#),
    ];
    turns.extend((0..4).map(|_| ScriptTurn::text(r#"{"verified":true,"match":2,"rejected":[]}"#)));
    let (record, requests) = work::run_observed(
        &env,
        turns,
        &["work", "../tasks.json", "--report-problems"],
        Some(path),
    );
    let reports = record["reports"].as_array().unwrap();
    assert_eq!(reports.len(), 5, "{record}");
    assert_eq!(reports[0]["disposition"], "filed", "{record}");
    for report in &reports[1..] {
        assert_eq!(report["disposition"], "existing_issue", "{record}");
    }
    assert_eq!(requests.len(), 6);
    #[cfg(target_os = "linux")]
    assert!(env.home().join("report-lease-inherited").is_file());
    let calls = fs::read_to_string(env.home().join("gh-calls")).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("issue create "))
            .count(),
        1
    );
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("issue comment 2 "))
            .count(),
        4
    );
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("search issues ") && line.contains("--state closed"))
            .count(),
        10
    );
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
fn later_publication_and_reporting_do_not_erase_execution_policy() {
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    let path = configure(&env);
    let mut turns = edits();
    turns[1] = ScriptTurn::text(concat!(
        "<oxa-work-reports>",
        r#"[{"title":"oxa work symptom","notes":"Synthetic reproduction","keywords":["oxa work","synthetic symptom"]}]"#,
        "</oxa-work-reports>",
    ));
    run_args(
        &env,
        turns,
        &["--offline", "work", "../tasks.json"],
        Some(path.clone()),
    );
    let state_path = state_path(&env);
    for flag in ["--publish", "--report-problems"] {
        let gh_calls = env.home().join("gh-calls");
        let before = fs::read(&gh_calls).unwrap_or_default();
        let record = run_args(&env, vec![], &["work", "../tasks.json", flag], Some(path.clone()));
        assert_eq!(record["outcome"], "done", "{record}");
        if flag == "--report-problems" {
            assert_eq!(record["reports"][0]["disposition"], "local", "{record}");
            assert_eq!(record["reports"][0]["detail"], "offline: report retained locally");
            assert_eq!(fs::read(gh_calls).unwrap_or_default(), before);
        }
        let state: serde_json::Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(state["offline"], true, "{flag} erased execution policy");
    }
}
#[test]
fn optional_publication_metadata_defaults_to_inactive() {
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    run(&env, vec![ScriptTurn::text("done")]);
    let state_path = state_path(&env);
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    for key in [
        "pull_request",
        "draft",
        "problem_notes",
        "reports",
        "acceptance_outcome",
    ] {
        state["record"].as_object_mut().unwrap().remove(key);
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
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
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
        setup(&env, r#"["test -f .oxa/settings.json"]"#);
        let path = configure(&env);
        env.write("tmp/gh", r##"#!/bin/sh
case "$1 $2" in
  'repo view') printf '%s' '{"nameWithOwner":"example/project","url":"https://github.com/example/project"}' ;;
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

fn write_turns(path: &str, content: &str) -> Vec<ScriptTurn> {
    let args = serde_json::json!({"path": path, "content": content}).to_string();
    vec![
        ScriptTurn::tool("write", "write", &args),
        ScriptTurn::text("done"),
    ]
}

/// Publication refuses before any GitHub request or push, and names only the place.
fn assert_refused(env: &TestEnv, record: &serde_json::Value, place: &str, secret: &str) {
    assert_eq!(record["outcome"], "blocked", "{record}");
    let detail = record["detail"].as_str().unwrap();
    assert!(detail.contains("publishing refused"), "{detail}");
    assert!(detail.contains(place), "{detail}");
    assert!(!record.to_string().contains(secret), "{record}");
    assert!(!env.home().join("push-calls").exists());
    assert!(!env.home().join("gh-calls").exists());
}

#[test]
fn publication_refuses_an_added_env_file() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let secret = format!("SERVICE_PASSWORD={}", "q".repeat(20));
    let record = run_args(
        &env,
        write_turns(".env", &format!("{secret}\n")),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_refused(&env, &record, ".env", &secret);
}

#[test]
fn publication_refuses_a_github_token_in_source() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let token = format!("ghp_{}", "x".repeat(36));
    let record = run_args(
        &env,
        write_turns(
            "src/client.txt",
            &format!("fn main() {{}}\nconst T: &str = \"{token}\";\n"),
        ),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_refused(&env, &record, "src/client.txt:2", &token);
}

#[test]
fn publication_refuses_a_private_key() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let body = "x".repeat(64);
    let key = format!(
        "-----BEGIN {kind}-----\n{body}\n-----END {kind}-----\n",
        kind = ["RSA", "PRIVATE KEY"].join(" ")
    );
    let record = run_args(
        &env,
        write_turns("deploy/notes.txt", &key),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_refused(&env, &record, "deploy/notes.txt:1", &body);
}

#[test]
fn publication_refuses_the_value_of_a_secret_environment_variable() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let value = format!("synthetic{}", "Q".repeat(12));
    let (record, _) = work::run_observed_with(
        &env,
        write_turns("answer.txt", &format!("copied {value}\n")),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
        |command| {
            command.env("SYNTHETIC_SERVICE_TOKEN", &value);
        },
    );
    assert_refused(&env, &record, "answer.txt:1", &value);
}

#[test]
fn publication_refuses_a_secret_that_a_later_commit_removed() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let path = configure(&env);
    let token = format!("ghp_{}", "x".repeat(36));
    let first = run_args(
        &env,
        write_turns("answer.txt", &format!("{token}\n")),
        &["work", "../tasks.json"],
        Some(path.clone()),
    );
    assert_eq!(first["outcome"], "done", "{first}");
    let record = run_args(
        &env,
        write_turns("answer.txt", "clean\n"),
        &["work", "../tasks.json", "--rerun", "--publish"],
        Some(path),
    );
    assert_ne!(record["head_sha"], first["head_sha"], "{record}");
    assert_refused(&env, &record, "answer.txt:1", &token);
}

#[test]
fn publication_refuses_an_empty_file_named_with_a_token() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let token = format!("ghp_{}", "x".repeat(36));
    let record = run_args(
        &env,
        write_turns(&format!("notes/{token}.txt"), ""),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_refused(&env, &record, "a file whose path", &token);
}

#[test]
fn publication_refuses_a_secret_in_a_local_base_commit_origin_lacks() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let repo = env.work().join("project");
    let token = format!("ghp_{}", "x".repeat(36));
    env.write("work/project/local.txt", &format!("{token}\n"));
    git(&env, &repo, &["add", "local.txt"]);
    git(&env, &repo, &["commit", "-m", "Local-only base commit"]);
    let record = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_refused(&env, &record, "local.txt:1", &token);
}

/// Refused after confirmation, from the destination's own tips, before any push.
fn assert_refused_before_push(env: &TestEnv, record: &serde_json::Value, place: &str, secret: &str) {
    assert_eq!(record["outcome"], "blocked", "{record}");
    let detail = record["detail"].as_str().unwrap();
    assert!(detail.contains("publishing refused"), "{detail}");
    assert!(detail.contains(place), "{detail}");
    assert!(!record.to_string().contains(secret), "{record}");
    assert!(!env.home().join("push-calls").exists());
}

fn local_secret_commit(env: &TestEnv, token: &str) {
    let repo = env.work().join("project");
    env.write("work/project/local.txt", &format!("{token}\n"));
    git(env, &repo, &["add", "local.txt"]);
    git(env, &repo, &["commit", "-m", "Local-only base commit"]);
}

#[test]
fn publication_refuses_a_secret_in_a_local_commit_message() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let repo = env.work().join("project");
    let token = format!("ghp_{}", "x".repeat(36));
    git(
        &env,
        &repo,
        &["commit", "--allow-empty", "-m", &format!("Note {token}")],
    );
    let record = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_refused(&env, &record, "commit ", &token);
}

/// Replace HEAD with a commit whose object carries `header` as an extra header.
fn craft_header_commit(env: &TestEnv, repo: &std::path::Path, header: &str) -> String {
    let raw = git(env, repo, &["cat-file", "commit", "HEAD"]);
    let (head, message) = raw.split_once("\n\n").unwrap();
    let file = env.tmp().join("crafted-commit");
    fs::write(&file, format!("{head}\n{header}\n\n{message}\n")).unwrap();
    let sha = git(
        env,
        repo,
        &[
            "hash-object",
            "-t",
            "commit",
            "-w",
            "--literally",
            file.to_str().unwrap(),
        ],
    );
    git(env, repo, &["reset", "--hard", "-q", &sha]);
    sha
}

fn assert_origin_lacks(env: &TestEnv, sha: &str) {
    let found = Command::new("git")
        .args(["--git-dir", "remote.git", "cat-file", "-e", sha])
        .current_dir(env.work())
        .status()
        .unwrap();
    assert!(!found.success(), "origin received {sha}");
}

#[test]
fn publication_refuses_a_secret_in_an_extra_commit_header() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let repo = env.work().join("project");
    let token = format!("ghp_{}", "x".repeat(36));
    let sha = craft_header_commit(&env, &repo, &format!("x-note {token}"));
    let record = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_refused_before_push(&env, &record, "commit ", &token);
    assert_origin_lacks(&env, &sha);
}

#[test]
fn publication_refuses_a_secret_in_a_multi_line_signature_header() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let repo = env.work().join("project");
    let token = format!("ghp_{}", "x".repeat(36));
    let header = format!("gpgsig -----BEGIN PGP SIGNATURE-----\n \n {token}\n -----END PGP SIGNATURE-----");
    let sha = craft_header_commit(&env, &repo, &header);
    let record = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_refused_before_push(&env, &record, "commit ", &token);
    assert_origin_lacks(&env, &sha);
}

#[test]
fn publication_ignores_forged_remote_tracking_refs() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let repo = env.work().join("project");
    let token = format!("ghp_{}", "x".repeat(36));
    local_secret_commit(&env, &token);
    // A local ref claims origin already has the secret commit; origin does not.
    git(&env, &repo, &["update-ref", "refs/remotes/origin/forged", "HEAD"]);
    let record = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_refused_before_push(&env, &record, "local.txt:1", &token);
}

#[test]
fn publication_checks_the_push_url_not_the_fetch_url() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let repo = env.work().join("project");
    let token = format!("ghp_{}", "x".repeat(36));
    local_secret_commit(&env, &token);
    // The fetch repository has the secret commit; the push destination does not.
    git(&env, &env.work(), &["init", "--bare", "mirror.git"]);
    git(&env, &repo, &["push", "../mirror.git", "main"]);
    git(&env, &repo, &["remote", "set-url", "origin", "../mirror.git"]);
    git(
        &env,
        &repo,
        &["remote", "set-url", "--push", "origin", "../remote.git"],
    );
    git(&env, &repo, &["fetch", "origin"]);
    let record = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_refused_before_push(&env, &record, "local.txt:1", &token);
}

#[test]
fn publication_never_pushes_tags_implicitly() {
    let env = TestEnv::new();
    setup(&env, r#"["true"]"#);
    let repo = env.work().join("project");
    let token = format!("ghp_{}", "x".repeat(36));
    git(&env, &repo, &["config", "push.followTags", "true"]);
    git(
        &env,
        &repo,
        &["tag", "-a", "synthetic-tag", "-m", &format!("Note {token}")],
    );
    let record = run_args(
        &env,
        edits(),
        &["work", "../tasks.json", "--publish"],
        Some(configure(&env)),
    );
    assert_eq!(record["outcome"], "done", "{record}");
    assert!(env.home().join("push-calls").exists());
    let tags = git(&env, &env.work(), &["--git-dir", "remote.git", "tag", "--list"]);
    assert!(tags.is_empty(), "{tags}");
}
