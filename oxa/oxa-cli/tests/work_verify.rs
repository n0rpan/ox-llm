//! Commit-bound acceptance through the real CLI and sandbox, with no publication.
#[path = "common/work.rs"]
mod work;
use oxa_fake::{ScriptTurn, TestEnv};
use std::{
    fs,
    net::{TcpListener, TcpStream},
    path::Path,
    process::Command,
};
use work::{git, run, run_args, setup};

fn verify(env: &TestEnv) -> std::process::Output {
    verify_with_home(env, &env.home())
}

fn verify_with_home(env: &TestEnv, home: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(["work", "verify", "test"])
        .current_dir(env.work().join("project"))
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("HOME", home)
        .output()
        .unwrap()
}

#[test]
fn a_clean_export_verifies_the_saved_head_tree_and_base_without_another_worker() {
    let env = TestEnv::new();
    setup(&env, r#"["test -f .oxa/settings.json"]"#);
    let record = run(&env, vec![ScriptTurn::text("done")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    let output = verify(&env);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let proof: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(proof["type"], "oxa_committed_acceptance");
    for key in ["head_sha", "tree_sha", "base_sha"] {
        assert_eq!(proof[key], record[key]);
    }
    assert!(proof["checked_at_ms"].as_u64().unwrap() > 0);
    assert!(git(
        &env,
        Path::new(record["worktree"].as_str().unwrap()),
        &["status", "--porcelain"]
    )
    .is_empty());
}

#[test]
fn a_retained_ignored_helper_cannot_make_the_committed_export_pass() {
    let env = TestEnv::new();
    setup(&env, r#"["sh helper.sh"]"#);
    let project = env.work().join("project");
    env.write("work/project/.gitignore", "helper.sh\n");
    git(&env, &project, &["add", ".gitignore"]);
    git(&env, &project, &["commit", "-m", "Ignore synthetic local helper"]);
    let record = run(
        &env,
        vec![
            ScriptTurn::tool("helper", "write", r#"{"path":"helper.sh","content":"exit 0\n"}"#),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    let retained = Path::new(record["worktree"].as_str().unwrap());
    let output = verify(&env);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("clean committed acceptance failed"));
    assert_eq!(
        fs::read_to_string(retained.join("helper.sh")).unwrap(),
        "exit 0\n"
    );
    assert_eq!(record["head_sha"], git(&env, retained, &["rev-parse", "HEAD"]));
}

#[test]
fn a_later_commit_cannot_reuse_the_saved_verdict() {
    let env = TestEnv::new();
    setup(&env, "[]");
    let record = run(&env, vec![ScriptTurn::text("done")]);
    let retained = Path::new(record["worktree"].as_str().unwrap());
    fs::write(retained.join("later.txt"), "later").unwrap();
    git(&env, retained, &["add", "later.txt"]);
    git(&env, retained, &["commit", "-m", "Synthetic later commit"]);
    let output = verify(&env);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("saved head or tree changed"));
    assert_eq!(fs::read_to_string(retained.join("later.txt")).unwrap(), "later");
}

#[test]
fn a_check_cannot_hide_changed_tracked_source_by_changing_its_index() {
    for conceal in [
        "git update-index --assume-unchanged payload.txt",
        "git update-index --skip-worktree payload.txt",
        "git rm --cached payload.txt",
    ] {
        let env = TestEnv::new();
        setup(&env, r#"["sh check.sh"]"#);
        let project = env.work().join("project");
        fs::write(project.join("payload.txt"), "approved\n").unwrap();
        fs::write(
            project.join("check.sh"),
            format!(
                "#!/bin/sh\nif ! git symbolic-ref -q HEAD >/dev/null; then\n  {conceal} || exit 1\n  printf 'forged\\n' >payload.txt\nfi\n"
            ),
        )
        .unwrap();
        git(&env, &project, &["add", "."]);
        git(&env, &project, &["commit", "-m", "Synthetic committed check"]);
        let record = run(&env, vec![ScriptTurn::text("done")]);
        assert_eq!(record["outcome"], "nothing_to_do", "{record}");
        let output = verify(&env);
        assert_eq!(output.status.code(), Some(2), "bypass: {conceal}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("acceptance changed committed source"));
        assert_eq!(
            fs::read_to_string(Path::new(record["worktree"].as_str().unwrap()).join("payload.txt")).unwrap(),
            "approved\n"
        );
    }
}

#[test]
fn checks_cannot_change_the_runners_committed_inspection_index() {
    let env = TestEnv::new();
    setup(&env, r#"["sh check.sh"]"#);
    let project = env.work().join("project");
    fs::write(project.join("payload.txt"), "approved\n").unwrap();
    fs::write(
        project.join("check.sh"),
        "#!/bin/sh\nif ! git symbolic-ref -q HEAD >/dev/null; then\n  test -r ../.committed-index || exit 1\n  if GIT_INDEX_FILE=../.committed-index git update-index --assume-unchanged payload.txt; then exit 1; fi\n  touch payload.txt\nfi\n",
    )
    .unwrap();
    git(&env, &project, &["add", "."]);
    git(
        &env,
        &project,
        &["commit", "-m", "Synthetic protected index check"],
    );
    let record = run(&env, vec![ScriptTurn::text("done")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    // Cover both an ordinary workspace and a workspace below a masked HOME.
    for home in [env.home(), env.work()] {
        let output = verify_with_home(&env, &home);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn checks_cannot_install_a_clean_filter_to_conceal_changed_source() {
    for configure_only in [true, false] {
        let env = TestEnv::new();
        setup(&env, r#"["sh check.sh"]"#);
        let project = env.work().join("project");
        fs::write(project.join("payload.txt"), "approved\n").unwrap();
        fs::write(project.join(".gitattributes"), "payload.txt filter=synthetic\n").unwrap();
        let configure = "git config filter.synthetic.clean \"printf 'approved\\n'\" 2>/dev/null";
        let attack = if configure_only {
            // A successful verifier also proves the real sandbox refused this
            // configuration write, independently of any final content diff.
            format!("if {configure}; then exit 1; fi")
        } else {
            format!("{configure} || :\ngit update-index --assume-unchanged payload.txt || exit 1\nprintf 'forged longer payload\\n' >payload.txt")
        };
        fs::write(
            project.join("check.sh"),
            format!("#!/bin/sh\nif ! git symbolic-ref -q HEAD >/dev/null; then\n{attack}\nfi\n"),
        )
        .unwrap();
        git(&env, &project, &["add", "."]);
        git(
            &env,
            &project,
            &["commit", "-m", "Synthetic filter concealment check"],
        );
        let record = run(&env, vec![ScriptTurn::text("done")]);
        assert_eq!(record["outcome"], "nothing_to_do", "{record}");
        let output = verify(&env);
        if configure_only {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        } else {
            assert_eq!(output.status.code(), Some(2));
            assert!(output.stdout.is_empty());
            assert!(String::from_utf8_lossy(&output.stderr).contains("acceptance changed committed source"));
        }
    }
}

#[test]
fn later_online_verification_keeps_the_saved_offline_check_policy() {
    let env = TestEnv::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let check = format!("if (: >/dev/tcp/127.0.0.1/{port}) 2>/dev/null; then exit 1; fi");
    setup(&env, &serde_json::to_string(&vec![check]).unwrap());
    let record = run_args(
        &env,
        vec![ScriptTurn::text("done")],
        &["--offline", "work", "../tasks.json"],
        None,
    );
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    assert!(
        TcpStream::connect(listener.local_addr().unwrap()).is_ok(),
        "online control is reachable"
    );
    let output = verify(&env);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_shallow_task_keeps_its_verified_boundary_without_fetching_missing_history() {
    let env = TestEnv::new();
    setup(
        &env,
        r#"["test \"$(git rev-parse --is-shallow-repository)\" = true"]"#,
    );
    let project = env.work().join("project");
    fs::write(project.join("second.txt"), "synthetic shallow head\n").unwrap();
    git(&env, &project, &["add", "."]);
    git(&env, &project, &["commit", "-m", "Synthetic shallow child"]);
    let seed = env.work().join("seed");
    fs::rename(&project, &seed).unwrap();
    let url = format!("file://{}", seed.display());
    git(
        &env,
        &env.work(),
        &["clone", "--depth=1", "--branch", "main", &url, "project"],
    );
    git(&env, &project, &["config", "user.name", "Test"]);
    git(&env, &project, &["config", "user.email", "test@example.invalid"]);
    let record = run(&env, vec![ScriptTurn::text("done")]);
    assert_eq!(record["outcome"], "nothing_to_do", "{record}");
    let output = verify(&env);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let proof: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(proof["head_sha"], record["head_sha"]);
    assert_eq!(
        git(&env, &project, &["rev-parse", "--is-shallow-repository"]),
        "true"
    );
}

#[test]
fn owner_state_is_refused_inside_worktrees_and_bare_metadata_before_any_task_action() {
    let env = TestEnv::new();
    setup(&env, "[]");
    for agent in [
        env.work().join("project/owner-state"),
        env.work().join("project/.git/owner-state"),
        env.work().join("remote.git/owner-state"),
        env.work().join(".oxa-worktrees/project/later/owner-state"),
    ] {
        fs::create_dir_all(&agent).unwrap();
        fs::write(agent.join("settings.json"), r#"{"defaultProjectTrust":"always"}"#).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(["work", "../tasks.json"])
            .current_dir(env.work().join("project"))
            .env_clear()
            .envs(env.envs("http://127.0.0.1:1"))
            .env("OXA_AGENT_DIR", &agent)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!agent.join("work").exists());
    }
}
