use oxa_fake::TestEnv;
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

fn invoke(env: &TestEnv, args: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(args.split_whitespace())
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs("http://127.0.0.1:1"))
        .env("GH_TOKEN", "synthetic")
        .env(
            "PATH",
            format!("{}:{}", env.tmp().display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap()
}

#[test]
fn real_cli_requires_opt_in_and_offline_never_reads_github() {
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
    for args in [
        "work board",
        "--offline work board --shadow --repo example/repo --once",
    ] {
        let out = invoke(&env, args);
        assert_eq!(out.status.code(), Some(2));
        assert!(!env.agent_dir().join("work").exists());
    }
    let gh = env.write("tmp/gh", r#"#!/bin/sh
case "$*" in
  *'pullRequests(first:100'*) printf '%s' '{"data":{"repository":{"pullRequests":{"totalCount":1,"nodes":[{"number":42}]}}}}' ;;
  *'p42:pullRequest(number:42)'*) cat detail.json ;;
  *) exit 92 ;;
esac
"#);
    fs::set_permissions(gh, fs::Permissions::from_mode(0o700)).unwrap();
    let mut detail: serde_json::Value = serde_json::from_str(r#"{"data":{"repository":{"p42":{"number":42,"headRefOid":"synthetic-head","state":"OPEN","body":"","isDraft":false,"reviewDecision":null,"mergeable":"CONFLICTING","labels":{"totalCount":0,"nodes":[]},"files":{"totalCount":0,"nodes":[]},"reviewThreads":{"totalCount":0,"nodes":[]},"latestOpinionatedReviews":{"totalCount":0,"nodes":[]},"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}}}}"#).unwrap();
    fs::write(env.work().join("detail.json"), detail.to_string()).unwrap();
    let out = invoke(&env, "work board --shadow --repo example/repo --once");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let storage = fs::read_dir(env.agent_dir().join("work"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let log_for = |repo: &str| {
        let bound = storage.join("board").join(repo).join("board-shadow.jsonl");
        if bound.exists() {
            bound
        } else {
            storage.join("board-shadow.jsonl")
        }
    };
    let log = log_for("example/repo");
    assert_eq!(fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
    let sample: serde_json::Value = serde_json::from_str(fs::read_to_string(log).unwrap().trim()).unwrap();
    assert_eq!(sample["would_take"][0]["action"], "resolve_conflict");
    assert_eq!(sample["board"]["entries"][0], sample["would_take"][0]);
    let mut actual = vec![];
    for (conclusion, head, source) in [
        ("STARTUP_FAILURE", "synthetic-head", ""),
        ("SUCCESS", "old-head", ""),
        ("SUCCESS", "synthetic-head", "engine.c"),
        ("SUCCESS", "synthetic-head", "view.qml"),
        ("SUCCESS", "synthetic-head", "header.hpp"),
    ] {
        let row = &mut detail["data"]["repository"]["p42"];
        row["mergeable"] = "MERGEABLE".into();
        row["reviewDecision"] = "APPROVED".into();
        row["latestOpinionatedReviews"] =
            serde_json::json!({"totalCount":1,"nodes":[{"state":"APPROVED","commit":{"oid":head}}]});
        row["commits"] = serde_json::json!({"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"totalCount":1,"nodes":[{"isRequired":true,"conclusion":conclusion}]}}}}]});
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
            "wait_review",
            "add_removes",
            "add_removes",
            "add_removes"
        ]
    );
    assert_ne!(log_for("example/repo"), log_for("example/other"));
}
