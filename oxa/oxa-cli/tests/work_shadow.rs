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
  *'p42:pullRequest(number:42)'*) printf '%s' '{"data":{"repository":{"p42":{"number":42,"headRefOid":"synthetic-head","state":"OPEN","body":"","isDraft":false,"reviewDecision":null,"mergeable":"CONFLICTING","labels":{"totalCount":0,"nodes":[]},"files":{"totalCount":0,"nodes":[]},"reviewThreads":{"totalCount":0,"nodes":[]},"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}}}}' ;;
  *) exit 92 ;;
esac
"#);
    fs::set_permissions(gh, fs::Permissions::from_mode(0o700)).unwrap();
    let out = invoke(&env, "work board --shadow --repo example/repo --once");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let storage = fs::read_dir(env.agent_dir().join("work"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let log = storage.join("board-shadow.jsonl");
    assert_eq!(fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
    let sample: serde_json::Value = serde_json::from_str(fs::read_to_string(log).unwrap().trim()).unwrap();
    assert_eq!(sample["would_take"][0]["action"], "resolve_conflict");
    assert_eq!(sample["board"]["entries"][0], sample["would_take"][0]);
}

#[test]
fn sampling_appends_and_waits_five_minutes_and_stops_on_read_error() {
    let env = TestEnv::new();
    let path = env.tmp().join("shadow.jsonl");
    let mut log = oxa_work::shadow::open_log(&path).unwrap();
    assert!(oxa_work::shadow::open_log(&path).is_err());
    let mut reads = 0;
    let mut waits = vec![];
    assert!(oxa_work::shadow::run(
        &mut log,
        false,
        || {
            reads += 1;
            if reads == 3 {
                return Err("synthetic read failure".into());
            }
            Ok(oxa_work::board::Board {
                observed_at: reads * 300,
                entries: vec![],
            })
        },
        |duration| waits.push(duration)
    )
    .is_err());
    assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 2);
    assert_eq!(waits.len(), 2);
    assert!(waits
        .iter()
        .all(|d| *d <= oxa_work::shadow::INTERVAL && d.as_secs() >= 299));
    drop(log);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(oxa_work::shadow::open_log(&path).is_err());
}
