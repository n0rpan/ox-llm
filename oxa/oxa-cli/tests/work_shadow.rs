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
if [ -e held-pipe ]; then
  (n=0; while [ ! -e release-pipe ] && [ "$n" -lt 450 ]; do sleep 0.1; n=$((n+1)); done) &
fi
case " $* " in *' --include '*) printf 'HTTP/2.0 200 OK\r\n\r\n' ;; esac
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
    detail["data"]["repository"]["pullRequests"]["nodes"][0]["headRefOid"] = "synthetic-head".into();
    detail["data"]["repository"]["pullRequests"]["pageInfo"] =
        serde_json::json!({"hasNextPage":false,"endCursor":"synthetic-end"});
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
        ("SUCCESS", "synthetic-head", "kernel.inc"),
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

fn batched_fixture(count: u64, limit: usize, failure: u16) -> TestEnv {
    let (env, detail) = fixture();
    fs::create_dir(env.work().join("pages")).unwrap();
    for start in (0..count.max(1)).step_by(100) {
        let end = count.min(start + 100);
        let connection = serde_json::json!({"totalCount":count,"nodes":(start+1..=end)
            .map(|number| serde_json::json!({"number":number,"headRefOid":"synthetic-head"})).collect::<Vec<_>>(),
            "pageInfo":{"hasNextPage":end<count,"endCursor":format!("c{end}")}});
        let cursor = if start == 0 {
            "first".into()
        } else {
            format!("c{start}")
        };
        fs::write(
            env.work().join(format!("pages/{cursor}.json")),
            connection.to_string(),
        )
        .unwrap();
    }
    fs::create_dir(env.work().join("rows")).unwrap();
    for number in 1..=count {
        let mut row = detail["data"]["repository"]["p42"].clone();
        row["number"] = number.into();
        fs::write(env.work().join(format!("rows/{number}.json")), row.to_string()).unwrap();
    }
    fs::write(env.work().join("limit"), limit.to_string()).unwrap();
    fs::write(env.work().join("failure"), failure.to_string()).unwrap();
    let gh = env.write("tmp/gh", r#"#!/bin/sh
include=false
case " $* " in *' --include '*) include=true ;; esac
cursor=$(printf '%s\n' "$*" | sed -n 's/.*after:"\([^"]*\)".*/\1/p')
numbers=$(printf '%s\n' "$*" | grep -oE 'p[0-9]+:pullRequest\(number:[0-9]+\)' | sed -E 's/.*number:([0-9]+)\)/\1/')
set -- $numbers
printf '%s\n' "$#" >> batch-sizes
status=200
if [ "$#" -gt "$(cat limit)" ]; then status=$(cat failure); fi
if [ -e fail-after ] && [ "$(wc -l < batch-sizes)" -gt "$(cat fail-after)" ]; then status=403; fi
if [ -e fail-page ] && [ "$cursor" = "$(cat fail-page)" ]; then status=403; fi
if "$include"; then printf 'HTTP/2.0 %s Synthetic\r\n\r\n' "$status"; fi
if [ -e response-padding ]; then
  head -c "$(cat response-padding)" /dev/zero | tr '\000' ' '
fi
if [ "$status" != 200 ]; then
  printf '{"message":"synthetic-private-response-marker"}'
  exit 1
fi
if [ -e advance-after-list ] && [ "$#" -gt 0 ]; then
  mv next-page.json pages/first.json
  rm advance-after-list
fi
printf '{"data":{"repository":{"pullRequests":'
if [ -e unstable-page.json ] && [ "$#" -gt 0 ]; then
  cat unstable-page.json
else
  cat "pages/${cursor:-first}.json"
fi
printf ',"mergeQueue":null'
for number in "$@"; do
  printf ',"p%s":' "$number"
  cat "rows/$number.json"
done
printf '}}}'
"#);
    fs::set_permissions(gh, fs::Permissions::from_mode(0o700)).unwrap();
    env
}

fn batch_sizes(env: &TestEnv) -> Vec<usize> {
    fs::read_to_string(env.work().join("batch-sizes"))
        .unwrap()
        .lines()
        .map(|line| line.parse().unwrap())
        .collect()
}

fn changing_membership_fixture() -> TestEnv {
    let env = batched_fixture(2, 15, 504);
    let page = env.work().join("pages/first.json");
    fs::copy(&page, env.work().join("next-page.json")).unwrap();
    let mut before: serde_json::Value = serde_json::from_slice(&fs::read(&page).unwrap()).unwrap();
    before["totalCount"] = 1.into();
    before["nodes"].as_array_mut().unwrap().truncate(1);
    before["pageInfo"]["endCursor"] = "c1".into();
    fs::write(page, before.to_string()).unwrap();
    fs::write(env.work().join("advance-after-list"), "").unwrap();
    env
}

#[test]
fn one_clean_reacquisition_uses_fresh_membership_and_publishes_once() {
    let env = changing_membership_fixture();
    let out = invoke(&env, "work board --shadow --repo example/repo --once");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(batch_sizes(&env), [0, 1, 0, 2, 0]);
    let text = fs::read_to_string(log_for(&env, "example/repo")).unwrap();
    assert_eq!(text.lines().count(), 1);
    let sample: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    let numbers: Vec<_> = sample["board"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["number"].as_u64().unwrap())
        .collect();
    assert_eq!(numbers, [1, 2]);
}

#[test]
fn persistent_change_gets_only_one_reacquisition_and_keeps_the_previous_sample() {
    let env = batched_fixture(2, 15, 504);
    let args = "work board --shadow --repo example/repo --once";
    assert!(invoke(&env, args).status.success());
    let previous = fs::read(log_for(&env, "example/repo")).unwrap();
    fs::write(env.work().join("batch-sizes"), "").unwrap();
    let mut changed: serde_json::Value =
        serde_json::from_slice(&fs::read(env.work().join("pages/first.json")).unwrap()).unwrap();
    changed["totalCount"] = 3.into();
    changed["nodes"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"number":3,"headRefOid":"synthetic-head"}));
    changed["pageInfo"]["endCursor"] = "c3".into();
    fs::write(env.work().join("unstable-page.json"), changed.to_string()).unwrap();
    assert!(!invoke(&env, args).status.success());
    assert_eq!(batch_sizes(&env), [0, 2, 0, 2]);
    assert_eq!(fs::read(log_for(&env, "example/repo")).unwrap(), previous);
}

#[test]
fn reacquisition_keeps_the_same_aggregate_response_budget() {
    let env = changing_membership_fixture();
    fs::write(env.work().join("response-padding"), "1500000").unwrap();
    let out = invoke(&env, "work board --shadow --repo example/repo --once");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("snapshot exceeds byte limit"));
    assert_eq!(batch_sizes(&env), [0, 1, 0]);
    assert!(fs::read(log_for(&env, "example/repo")).unwrap().is_empty());
}

#[test]
fn transient_http_output_uses_the_same_snapshot_byte_budget() {
    let env = batched_fixture(31, 7, 504);
    fs::write(env.work().join("response-padding"), "800000").unwrap();
    let out = invoke(&env, "work board --shadow --repo example/repo --once");
    assert!(!out.status.success());
    assert_eq!(batch_sizes(&env), [0, 15, 15, 15, 7, 7]);
    assert!(fs::read(log_for(&env, "example/repo")).unwrap().is_empty());
}

#[test]
fn a_wrong_detail_number_does_not_gain_a_consistency_retry() {
    let env = batched_fixture(1, 15, 504);
    let path = env.work().join("rows/1.json");
    let mut row: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    row["number"] = 2.into();
    fs::write(path, row.to_string()).unwrap();
    assert!(!invoke(&env, "work board --shadow --repo example/repo --once")
        .status
        .success());
    assert_eq!(batch_sizes(&env), [0, 1]);
    assert!(fs::read(log_for(&env, "example/repo")).unwrap().is_empty());
}

#[test]
fn malformed_membership_is_not_a_consistency_retry() {
    let env = batched_fixture(2, 15, 504);
    let mut malformed: serde_json::Value =
        serde_json::from_slice(&fs::read(env.work().join("pages/first.json")).unwrap()).unwrap();
    malformed["nodes"][0]["headRefOid"] = serde_json::Value::Null;
    fs::write(env.work().join("unstable-page.json"), malformed.to_string()).unwrap();
    assert!(!invoke(&env, "work board --shadow --repo example/repo --once")
        .status
        .success());
    assert_eq!(batch_sizes(&env), [0, 2]);
    assert!(fs::read(log_for(&env, "example/repo")).unwrap().is_empty());
}

#[test]
fn ninety_pr_details_are_read_in_small_batches_without_losing_rows() {
    let env = batched_fixture(90, 15, 504);
    let out = invoke(&env, "work board --shadow --repo example/repo --once");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let sizes = batch_sizes(&env);
    assert_eq!(sizes[0], 0);
    assert_eq!(sizes.last(), Some(&0));
    assert!(sizes[1..sizes.len() - 1]
        .iter()
        .all(|size| *size > 0 && *size <= 15));
    assert_eq!(sizes.iter().sum::<usize>(), 90);
    let text = fs::read_to_string(log_for(&env, "example/repo")).unwrap();
    let sample: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    let numbers: Vec<_> = sample["board"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["number"].as_u64().unwrap())
        .collect();
    assert_eq!(numbers, (1..=90).collect::<Vec<_>>());
}

#[test]
fn repeated_gateway_failures_retry_then_reduce_the_detail_batch() {
    for status in [502, 503, 504] {
        let env = batched_fixture(31, 7, status);
        let out = invoke(&env, "work board --shadow --repo example/repo --once");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let sizes = batch_sizes(&env);
        assert_eq!(&sizes[..5], &[0, 15, 15, 15, 7]);
        assert_eq!(sizes.last(), Some(&0));
        assert!(sizes[4..sizes.len() - 1]
            .iter()
            .all(|size| *size > 0 && *size <= 7));
    }
}

#[test]
fn terminal_http_failures_are_bounded_named_and_do_not_publish_a_partial_board() {
    for (status, attempts) in [(401, 1), (403, 1), (429, 1), (504, 3)] {
        let env = batched_fixture(1, 0, status);
        let out = invoke(&env, "work board --shadow --repo example/repo --once");
        assert!(!out.status.success());
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(error.contains(&format!("HTTP {status}")), "{error}");
        assert!(!error.contains("synthetic"));
        assert_eq!(batch_sizes(&env), [vec![0], vec![1; attempts]].concat());
        assert!(fs::read_to_string(log_for(&env, "example/repo"))
            .unwrap()
            .is_empty());
    }
    let env = batched_fixture(31, 15, 504);
    fs::write(env.work().join("fail-after"), "2").unwrap();
    let out = invoke(&env, "work board --shadow --repo example/repo --once");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("HTTP 403"));
    assert_eq!(batch_sizes(&env), [0, 15, 15]);
    assert!(fs::read_to_string(log_for(&env, "example/repo"))
        .unwrap()
        .is_empty());
}

#[test]
fn complete_paginated_boards_include_every_pr_up_to_the_limit() {
    for count in [101, 1000] {
        let env = batched_fixture(count, 15, 504);
        let out = invoke(&env, "work board --shadow --repo example/repo --once");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let text = fs::read_to_string(log_for(&env, "example/repo")).unwrap();
        let sample: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        let numbers: Vec<_> = sample["board"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["number"].as_u64().unwrap())
            .collect();
        assert_eq!(numbers, (1..=count).collect::<Vec<_>>());
        assert_eq!(batch_sizes(&env).iter().sum::<usize>(), count as usize);
    }
}

#[test]
fn later_page_failures_and_changed_heads_never_publish_partial_boards() {
    for fault in ["http", "late-http", "duplicate", "head", "cursor", "identity"] {
        let env = batched_fixture(101, 15, 504);
        if fault == "http" {
            fs::write(env.work().join("fail-page"), "c100").unwrap();
        } else if fault == "late-http" {
            fs::write(env.work().join("fail-after"), "10").unwrap();
        } else if matches!(fault, "duplicate" | "cursor" | "identity") {
            let path = env.work().join("pages/c100.json");
            let mut page: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            match fault {
                "duplicate" => page["nodes"][0]["number"] = 1.into(),
                "cursor" => page["pageInfo"]["hasNextPage"] = true.into(),
                _ => page["nodes"][0]["headRefOid"] = serde_json::Value::Null,
            }
            fs::write(path, page.to_string()).unwrap();
        } else {
            let path = env.work().join("rows/101.json");
            let mut row: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            row["headRefOid"] = "changed-head".into();
            fs::write(path, row.to_string()).unwrap();
        }
        let out = invoke(&env, "work board --shadow --repo example/repo --once");
        assert!(!out.status.success(), "{fault}");
        if fault == "late-http" {
            assert!(String::from_utf8_lossy(&out.stderr).contains("HTTP 403"));
            assert_eq!(batch_sizes(&env).iter().sum::<usize>(), 101);
        }
        assert!(fs::read_to_string(log_for(&env, "example/repo"))
            .unwrap()
            .is_empty());
    }
}

#[test]
fn exited_github_child_cannot_escape_the_whole_read_deadline() {
    use std::time::{Duration, Instant};
    let (env, _) = fixture();
    fs::write(env.work().join("held-pipe"), "synthetic").unwrap();
    let mut child = command(&env, "work board --shadow --repo example/repo --once")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let ended = loop {
        if child.try_wait().unwrap().is_some() {
            break true;
        }
        if start.elapsed() >= Duration::from_secs(35) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // Release the finite peer before assertions, including the before-fix failure.
    fs::write(env.work().join("release-pipe"), "release").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        ended,
        "a descendant-held pipe bypassed the 30-second read deadline"
    );
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(fs::read_to_string(env.work().join("gh-calls")).unwrap(), "read\n");
    assert!(
        !log_for(&env, "example/repo").exists()
            || fs::metadata(log_for(&env, "example/repo")).unwrap().len() == 0
    );
}

#[test]
fn opt_in_review_policy_observes_pinned_diff_and_authenticated_reviews() {
    let (env, mut detail) = fixture();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(env.work())
            .env_clear()
            .envs(env.envs("http://127.0.0.1:1"))
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    git(&["commit", "--allow-empty", "-m", "base"]);
    let base = git(&["rev-parse", "HEAD"]);
    env.write("work/engine/core.rs", "fn change() {}\n");
    git(&["add", "engine/core.rs"]);
    git(&["commit", "-m", "own diff"]);
    let head = git(&["rev-parse", "HEAD"]);
    env.write(
        "home/review.json",
        r#"{"requester":"review-bot","codex":"code-reviewer","security":"security-reviewer","owner":"owner-reviewer","core":["engine"],"protected":[]}"#,
    );
    detail["data"]["repository"]["pullRequests"]["nodes"][0]["headRefOid"] = head.clone().into();
    let row = &mut detail["data"]["repository"]["p42"];
    row["baseRefOid"] = base.into();
    row["headRefOid"] = head.clone().into();
    row["authorAssociation"] = "MEMBER".into();
    row["isCrossRepository"] = false.into();
    row["mergeable"] = "MERGEABLE".into();
    row["comments"] = serde_json::json!({ "totalCount": 0, "nodes": [] });
    row["commits"] = serde_json::json!({ "nodes": [{ "commit": {"statusCheckRollup":{"contexts":
        {"totalCount":1,"nodes":[{"name":"ci","isRequired":true,"conclusion":"SUCCESS"}]}
    }}}] });
    row["latestOpinionatedReviews"] = serde_json::json!({
        "totalCount": 2,
        "nodes": [
            {"state":"APPROVED","commit":{"oid":head},"author":{"login":"code-reviewer"}},
            {"state":"APPROVED","commit":{"oid":head},"author":{"login":"outside"}}
        ]
    });
    fs::write(env.work().join("detail.json"), detail.to_string()).unwrap();
    fs::write(env.work().join("first.json"), detail.to_string()).unwrap();
    let out = invoke(
        &env,
        "work board --shadow --repo example/repo --once --review-policy ../home/review.json",
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let log = fs::read_to_string(log_for(&env, "example/repo")).unwrap();
    let sample: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(sample["board"]["entries"][0]["action"], "wait_review");
    let plan = &sample["reviews"]["42"];
    assert_eq!(plan["decision"]["review"]["tier"], "core");
    assert_eq!(
        plan["decision"]["review"]["outstanding"],
        serde_json::json!(["security"])
    );
    assert_eq!(plan["requests"][0]["key"]["reviewer"], "security");
    assert_eq!(plan["requests"][0]["head"], head);

    let receipt = format!(
        "<!-- codex-review-receipt:v1 -->\n\nCodex Review: Didn't find any major issues. :rocket:\n\n**Reviewed commit:** `{}`",
        &head[..10]
    );
    for (actor, edited, attached, outstanding) in [
        (
            "outside",
            serde_json::Value::Null,
            true,
            serde_json::json!(["codex", "security"]),
        ),
        (
            "code-reviewer",
            serde_json::json!("2026-01-01T00:00:00Z"),
            true,
            serde_json::json!(["codex", "security"]),
        ),
        (
            "code-reviewer",
            serde_json::Value::Null,
            false,
            serde_json::json!(["codex", "security"]),
        ),
        (
            "code-reviewer",
            serde_json::Value::Null,
            true,
            serde_json::json!(["security"]),
        ),
    ] {
        let row = &mut detail["data"]["repository"]["p42"];
        row["latestOpinionatedReviews"] = serde_json::json!({"totalCount":0,"nodes":[]});
        row["comments"] = serde_json::json!({"totalCount":1,"nodes":[{
            "body":receipt,"author":{"login":actor},"lastEditedAt":edited
        }]});
        row["reviews"] = if attached {
            serde_json::json!({"totalCount":1,"nodes":[{
                "state":"COMMENTED","body":receipt,"author":{"login":actor},
                "lastEditedAt":edited,"commit":{"oid":head}
            }]})
        } else {
            serde_json::json!({"totalCount":0,"nodes":[]})
        };
        fs::write(env.work().join("detail.json"), detail.to_string()).unwrap();
        let out = invoke(
            &env,
            "work board --shadow --repo example/repo --once --review-policy ../home/review.json",
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let log = fs::read_to_string(log_for(&env, "example/repo")).unwrap();
        let sample: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(
            sample["reviews"]["42"]["decision"]["review"]["outstanding"],
            outstanding
        );
    }
}
