use oxa_fake::TestEnv;
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

fn command(env: &TestEnv, args: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .args(args.split_whitespace())
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("GH_TOKEN", "synthetic")
        .env(
            "PATH",
            format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap()),
        );
    command
}

fn invoke(env: &TestEnv, args: &str) -> std::process::Output {
    command(env, args).output().unwrap()
}

fn fixture() -> (TestEnv, serde_json::Value) {
    let env = TestEnv::new();
    assert!(Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(env.work())
        .output()
        .unwrap()
        .status
        .success());
    fs::set_permissions(env.tmp(), fs::Permissions::from_mode(0o700)).unwrap();
    oxa_perm::TrustStore::default()
        .save(&env.agent_dir().join("trust.json"), &env.work(), true)
        .unwrap();
    let gh = env.write("tmp/gh", r#"#!/bin/sh
printf 'read\n' >> gh-calls
if [ -e expect-input ]; then IFS= read -r expected < expect-input; [ "$GH_TOKEN" = "$expected" ] || exit 93; fi
if [ -e oversized ]; then
  printf '{"data":{"repository":{"pullRequests":{"totalCount":0,"nodes":[]},"padding":"'
  head -c 4194305 /dev/zero | tr '\000' 'a'
  printf '"}}}'
  exit 0
fi
case "$*" in
  *'p42:pullRequest(number:42)'*) cat detail.json ;;
  *'pullRequests(first:100'*) cat first.json ;;
  *) exit 92 ;;
esac
"#);
    fs::set_permissions(gh, fs::Permissions::from_mode(0o700)).unwrap();
    let detail: serde_json::Value = serde_json::from_str(r#"{"data":{"repository":{"pullRequests":{"totalCount":1,"nodes":[{"number":42}]},"mergeQueue":null,"p42":{"number":42,"headRefOid":"synthetic-head","baseRefName":"main","state":"OPEN","body":"","isDraft":false,"reviewDecision":null,"mergeable":"CONFLICTING","mergeStateStatus":"CLEAN","labels":{"totalCount":0,"nodes":[]},"files":{"totalCount":0,"nodes":[]},"reviewThreads":{"totalCount":0,"nodes":[]},"latestOpinionatedReviews":{"totalCount":0,"nodes":[]},"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}}}}"#).unwrap();
    let mut detail = detail;
    detail["data"]["repository"]["p42"]["baseRef"] = serde_json::json!({
        "refUpdateRule":{"requiredStatusCheckContexts":["ci"]},
        "rules":{"totalCount":0,"nodes":[]}
    });
    fs::write(env.work().join("detail.json"), detail.to_string()).unwrap();
    fs::write(env.work().join("first.json"), detail.to_string()).unwrap();
    (env, detail)
}

fn log_for(env: &TestEnv, repo: &str) -> std::path::PathBuf {
    fs::read_dir(env.agent_dir().join("work"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("board")
        .join(repo)
        .join("board-shadow.jsonl")
}

fn sample(env: &TestEnv, detail: &serde_json::Value) -> serde_json::Value {
    fs::write(env.work().join("detail.json"), detail.to_string()).unwrap();
    let out = invoke(env, "work board --shadow --repo example/repo --once");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = fs::read_to_string(log_for(env, "example/repo")).unwrap();
    serde_json::from_str(text.lines().last().unwrap()).unwrap()
}

#[test]
fn real_cli_requires_opt_in_and_offline_never_reads_github() {
    let (env, mut detail) = fixture();
    for args in [
        "work board",
        "--offline work board --shadow --repo example/repo --once",
    ] {
        assert_eq!(invoke(&env, args).status.code(), Some(2));
        assert!(!env.agent_dir().join("work").exists());
    }
    assert!(!env.work().join("gh-calls").exists());
    let out = invoke(&env, "work board --shadow --repo example/repo --once");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let storage = fs::read_dir(env.agent_dir().join("work"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let log_for = |repo: &str| storage.join("board").join(repo).join("board-shadow.jsonl");
    let log = log_for("example/repo");
    assert_eq!(fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
    let sample: serde_json::Value = serde_json::from_str(fs::read_to_string(log).unwrap().trim()).unwrap();
    assert_eq!(sample["would_take"][0]["action"], "resolve_conflict");
    assert_eq!(sample["board"]["entries"][0], sample["would_take"][0]);
    let mut actual = vec![];
    for (conclusion, head, source) in [
        ("STARTUP_FAILURE", "synthetic-head", ""),
        ("STALE", "synthetic-head", ""),
        ("SUCCESS", "old-head", ""),
        ("SUCCESS", "synthetic-head", ""),
        ("SUCCESS", "synthetic-head", "engine.c"),
        ("SUCCESS", "synthetic-head", "view.qml"),
        ("SUCCESS", "synthetic-head", "header.hpp"),
    ] {
        let row = &mut detail["data"]["repository"]["p42"];
        row["mergeable"] = "MERGEABLE".into();
        row["reviewDecision"] = "APPROVED".into();
        row["latestOpinionatedReviews"] =
            serde_json::json!({"totalCount":1,"nodes":[{"state":"APPROVED","commit":{"oid":head}}]});
        row["commits"] = serde_json::json!({"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"totalCount":1,"nodes":[{"name":"ci","isRequired":true,"conclusion":conclusion}]}}}}]});
        row["files"] = if source.is_empty() {
            serde_json::json!({"totalCount":0,"nodes":[]})
        } else {
            serde_json::json!({"totalCount":1,"nodes":[{"path":source,"deletions":1}]})
        };
        fs::write(env.work().join("detail.json"), detail.to_string()).unwrap();
        assert!(invoke(&env, "work board --shadow --repo example/repo --once")
            .status
            .success());
        let text = fs::read_to_string(log_for("example/repo")).unwrap();
        let sample: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        actual.push(
            sample["board"]["entries"][0]["action"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert!(invoke(&env, "work board --shadow --repo example/other --once")
        .status
        .success());
    assert_eq!(
        actual,
        vec![
            "fix_required_ci",
            "fix_required_ci",
            "wait_review",
            "ready",
            "add_removes",
            "add_removes",
            "add_removes"
        ]
    );
    assert_eq!(
        fs::read_to_string(log_for("example/other"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn requested_change_summary_is_actionable_without_threads() {
    let (env, mut detail) = fixture();
    detail["data"]["repository"]["p42"]["reviewDecision"] = "CHANGES_REQUESTED".into();
    assert_eq!(
        sample(&env, &detail)["board"]["entries"][0]["action"],
        "fix_findings"
    );
}

#[test]
fn changed_open_set_is_refused() {
    let (env, mut detail) = fixture();
    detail["data"]["repository"]["pullRequests"] =
        serde_json::json!({"totalCount":2,"nodes":[{"number":42},{"number":43}]});
    fs::write(env.work().join("detail.json"), detail.to_string()).unwrap();
    assert_eq!(
        invoke(&env, "work board --shadow --repo example/repo --once")
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn behind_heads_wait_without_a_queue_and_keep_queue_composition() {
    let (env, mut detail) = fixture();
    let row = &mut detail["data"]["repository"]["p42"];
    row["mergeable"] = "MERGEABLE".into();
    row["mergeStateStatus"] = "BEHIND".into();
    row["reviewDecision"] = "APPROVED".into();
    row["latestOpinionatedReviews"] =
        serde_json::json!({"totalCount":1,"nodes":[{"state":"APPROVED","commit":{"oid":"synthetic-head"}}]});
    row["commits"] = serde_json::json!({"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"totalCount":1,"nodes":[{"name":"ci","isRequired":true,"conclusion":"SUCCESS"}]}}}}]});
    let mut actions = vec![];
    for queued in [false, true] {
        let mut first = detail.clone();
        first["data"]["repository"]["mergeQueue"] = if queued {
            serde_json::json!({"id":"synthetic-queue"})
        } else {
            serde_json::Value::Null
        };
        fs::write(env.work().join("first.json"), first.to_string()).unwrap();
        actions.push(
            sample(&env, &detail)["board"]["entries"][0]["action"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert_eq!(actions, vec!["wait_mergeability", "ready"]);
}

#[test]
fn oversized_valid_json_is_refused_before_parsing() {
    let (env, _) = fixture();
    fs::write(env.work().join("oversized"), "synthetic").unwrap();
    assert_eq!(
        invoke(&env, "work board --shadow --repo example/repo --once")
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn board_help_needs_no_repository_or_private_state() {
    let env = TestEnv::new();
    for args in ["work board --help", "help work board"] {
        let out = invoke(&env, args);
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("board --shadow"));
    }
    assert!(!env.agent_dir().join("work").exists());
}

#[test]
fn refreshed_token_can_be_supplied_privately_by_the_caller() {
    let (env, _) = fixture();
    fs::write(env.work().join("expect-input"), "synthetic-refreshed\n").unwrap();
    let mut child = command(
        &env,
        "work board --shadow --repo example/repo --once --token-stdin",
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"synthetic-refreshed\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn approval_required_block_has_an_actionable_wait_reason() {
    let (env, mut detail) = fixture();
    let row = &mut detail["data"]["repository"]["p42"];
    row["mergeable"] = "MERGEABLE".into();
    row["mergeStateStatus"] = "BLOCKED".into();
    row["reviewDecision"] = "REVIEW_REQUIRED".into();
    row["commits"] = serde_json::json!({"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"totalCount":1,"nodes":[{"name":"ci","isRequired":true,"conclusion":"SUCCESS"}]}}}}]});
    assert_eq!(
        sample(&env, &detail)["board"]["entries"][0]["action"],
        "wait_review"
    );
}

#[test]
fn unreported_classic_and_ruleset_requirements_are_pending() {
    let (env, mut detail) = fixture();
    let row = &mut detail["data"]["repository"]["p42"];
    row["mergeable"] = "MERGEABLE".into();
    row["commits"] = serde_json::json!({"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"totalCount":1,"nodes":[{"name":"ci","isRequired":true,"conclusion":"SUCCESS"}]}}}}]});
    for base in [
        serde_json::json!({"refUpdateRule":{"requiredStatusCheckContexts":["ci","missing"]},"rules":{"totalCount":0,"nodes":[]}}),
        serde_json::json!({"refUpdateRule":null,"rules":{"totalCount":1,"nodes":[{"type":"REQUIRED_STATUS_CHECKS","parameters":{"requiredStatusChecks":[{"context":"ci"},{"context":"missing"}]}}]}}),
    ] {
        detail["data"]["repository"]["p42"]["baseRef"] = base;
        assert_eq!(
            sample(&env, &detail)["board"]["entries"][0]["action"],
            "wait_required_ci"
        );
    }
}
