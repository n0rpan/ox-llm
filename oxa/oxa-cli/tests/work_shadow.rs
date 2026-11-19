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
case " $* " in
  *' --include '*)
    printf 'HTTP/2.0 200 OK\r\n'
    if [ -e app-quota-headers ]; then cat app-quota-headers; fi
    printf '\r\n'
    ;;
esac
if [ -e oversized ]; then
  printf '{"data":{"repository":{"pullRequests":{"totalCount":0,"nodes":[]},"padding":"'
  head -c 4194305 /dev/zero | tr '\000' 'a'
  printf '"}}}'
  exit 0
fi
case "$*" in
  *'actions/runs/61/attempts/1/jobs?per_page=100'*) cat report-jobs.json ;;
  *'actions/runs/61/attempts/1'*) printf 'read\n' >> report-attempt-reads; cat report-attempt.json ;;
  *'actions/runs/61/artifacts?per_page=100'*) cat report-artifacts.json ;;
  *'actions/artifacts/71/zip'*) printf 'read\n' >> report-reads; cat report.zip ;;
  *'enqueuedAt headCommit'*) cat queue-state.json ;;
  *'comments(last:100)'*) cat review.json ;;
  *'REMOVED_FROM_MERGE_QUEUE_EVENT'*) printf 'read\n' >> queue-calls; cat queue.json ;;
  *'p42:pullRequest(number:42)'*) cat detail.json ;;
  *'pullRequests(first:100'*) cat first.json ;;
  *) exit 92 ;;
esac
"#);
    fs::set_permissions(gh, fs::Permissions::from_mode(0o700)).unwrap();
    let detail: serde_json::Value = serde_json::from_str(r#"{"data":{"repository":{"pullRequests":{"totalCount":1,"nodes":[{"number":42}]},"mergeQueue":null,"p42":{"number":42,"headRefOid":"synthetic-head","baseRefName":"main","state":"OPEN","body":"","isDraft":false,"reviewDecision":null,"mergeable":"CONFLICTING","mergeStateStatus":"CLEAN","labels":{"totalCount":0,"nodes":[]},"files":{"totalCount":0,"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}},"reviewThreads":{"totalCount":0,"nodes":[]},"latestOpinionatedReviews":{"totalCount":0,"nodes":[]},"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}}}}"#).unwrap();
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
fn shared_or_redirected_board_storage_is_refused_before_github() {
    let mut accepted = Vec::new();
    for fault in ["readable", "writable", "symlink"] {
        let (env, _) = fixture();
        let args = "work board --shadow --repo example/repo --once";
        assert!(invoke(&env, args).status.success());
        let log = log_for(&env, "example/repo");
        let directory = log.parent().unwrap();
        let before = fs::read(&log).unwrap();
        let calls = fs::read(env.work().join("gh-calls")).unwrap();
        if fault == "symlink" {
            let target = directory.with_file_name("retained-board-storage");
            fs::rename(directory, &target).unwrap();
            std::os::unix::fs::symlink(target, directory).unwrap();
        } else {
            let mode = if fault == "readable" { 0o755 } else { 0o770 };
            fs::set_permissions(directory, fs::Permissions::from_mode(mode)).unwrap();
        }
        let out = invoke(&env, args);
        if out.status.success()
            || fs::read(&log).unwrap() != before
            || fs::read(env.work().join("gh-calls")).unwrap() != calls
        {
            accepted.push(fault);
        }
    }
    assert!(accepted.is_empty(), "unprotected storage accepted: {accepted:?}");
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
    for (status, conclusion, head, source) in [
        ("COMPLETED", "STARTUP_FAILURE", "synthetic-head", ""),
        ("COMPLETED", "STALE", "synthetic-head", ""),
        ("IN_PROGRESS", "SUCCESS", "synthetic-head", ""),
        ("COMPLETED", "SUCCESS", "old-head", ""),
        ("COMPLETED", "SUCCESS", "synthetic-head", ""),
        ("COMPLETED", "SUCCESS", "synthetic-head", "engine.c"),
        ("COMPLETED", "SUCCESS", "synthetic-head", "view.qml"),
        ("COMPLETED", "SUCCESS", "synthetic-head", "header.hpp"),
        ("COMPLETED", "SUCCESS", "synthetic-head", "kernel.inc"),
    ] {
        let row = &mut detail["data"]["repository"]["p42"];
        row["mergeable"] = "MERGEABLE".into();
        row["reviewDecision"] = "APPROVED".into();
        row["latestOpinionatedReviews"] =
            serde_json::json!({"totalCount":1,"nodes":[{"state":"APPROVED","commit":{"oid":head}}]});
        row["commits"] = serde_json::json!({"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"totalCount":1,"nodes":[{"name":"ci","isRequired":true,"status":status,"conclusion":conclusion}]}}}}]});
        row["files"] = if source.is_empty() {
            serde_json::json!({"totalCount":0,"nodes":[],
                "pageInfo":{"hasNextPage":false,"endCursor":null}})
        } else {
            serde_json::json!({"totalCount":1,"nodes":[{"path":source,"deletions":1}],
                "pageInfo":{"hasNextPage":false,"endCursor":null}})
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
            "wait_required_ci",
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
    row["commits"] = serde_json::json!({"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"totalCount":1,"nodes":[{"name":"ci","isRequired":true,"status":"COMPLETED","conclusion":"SUCCESS"}]}}}}]});
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
    let resets_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 600;
    fs::write(
        env.work().join("app-quota-headers"),
        format!("X-RateLimit-Resource: graphql\r\nX-RateLimit-Limit: 5000\r\nX-RateLimit-Remaining: 123\r\nX-RateLimit-Reset: {resets_at}\r\n"),
    )
    .unwrap();
    let args = "work board --shadow --repo example/repo --once";
    assert!(!invoke(&env, &format!("{args} --app-quota-shadow"))
        .status
        .success());
    assert!(invoke(&env, args).status.success());
    fs::write(env.work().join("expect-input"), "synthetic-refreshed\n").unwrap();
    let mut child = command(&env, &format!("{args} --token-stdin --app-quota-shadow"))
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
    let log = fs::read_to_string(log_for(&env, "example/repo")).unwrap();
    let mut samples = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap());
    assert!(samples.next().unwrap().get("app_quota").is_none());
    assert_eq!(
        samples.next().unwrap()["app_quota"],
        serde_json::json!({
            "core": null,
            "graphql": {"limit": 5000, "remaining": 123, "resets_at": resets_at},
        })
    );
    assert!(samples.next().is_none());
    assert!(!log.contains("synthetic-refreshed"));
}

#[test]
fn approval_required_block_has_an_actionable_wait_reason() {
    let (env, mut detail) = fixture();
    let row = &mut detail["data"]["repository"]["p42"];
    row["mergeable"] = "MERGEABLE".into();
    row["mergeStateStatus"] = "BLOCKED".into();
    row["reviewDecision"] = "REVIEW_REQUIRED".into();
    row["commits"] = serde_json::json!({"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"totalCount":1,"nodes":[{"name":"ci","isRequired":true,"status":"COMPLETED","conclusion":"SUCCESS"}]}}}}]});
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
    row["commits"] = serde_json::json!({"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"totalCount":1,"nodes":[{"name":"ci","isRequired":true,"status":"COMPLETED","conclusion":"SUCCESS"}]}}}}]});
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
        r#"{"requester":"review-bot","codex":"code-reviewer","security":"security-reviewer","owner":"owner-reviewer","core":["engine"],"protected":["docs/protected"]}"#,
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
        {"totalCount":1,"nodes":[{"name":"ci","isRequired":true,"status":"COMPLETED","conclusion":"SUCCESS"}]}
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
    env.write(
        "work/queue.json",
        r#"{"data":{"repository":{"mergeQueue":null}}}"#,
    );
    env.write(
        "work/queue-state.json",
        r#"{"data":{"repository":{"mergeQueue":null}}}"#,
    );
    fs::write(env.work().join("review.json"), detail.to_string()).unwrap();
    let out = invoke(
        &env,
        "work board --shadow --repo example/repo --once --review-policy ../home/review.json --landing-shadow",
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let log = fs::read_to_string(log_for(&env, "example/repo")).unwrap();
    let sample: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(sample["board"]["entries"][0]["action"], "wait_review");
    assert_eq!(sample["landing"]["42"]["next"], "wait_review");
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

    for (commit, author_id, editor_id, outstanding) in [
        (&head, 1, 199_175_422, serde_json::json!(["codex", "security"])),
        (&head, 199_175_422, 1, serde_json::json!(["codex", "security"])),
        (
            &"0".repeat(40),
            199_175_422,
            199_175_422,
            serde_json::json!(["codex", "security"]),
        ),
        (&head, 199_175_422, 199_175_422, serde_json::json!(["security"])),
    ] {
        let state = serde_json::json!({
            "code_review": {
                "commit_sha": commit, "status": "completed",
                "has_any_findings": false, "has_unresolved_findings": false
            },
            "security_review": {
                "commit_sha": commit, "status": "completed",
                "has_any_findings": null, "has_unresolved_findings": null
            }
        });
        let summary =
            format!("<!-- codex-pull-request-review-summary -->\n<!-- codex-review-state:v1 {state} -->");
        let row = &mut detail["data"]["repository"]["p42"];
        row["reviews"] = serde_json::json!({"totalCount":0,"nodes":[]});
        row["comments"] = serde_json::json!({"totalCount":1,"nodes":[{
            "body":summary,
            "author":{"login":"code-reviewer","databaseId":author_id},
            "editor":{"login":"code-reviewer","databaseId":editor_id},
            "lastEditedAt":"2026-01-01T00:00:00Z",
            "updatedAt":"2026-01-01T00:00:01Z"
        }]});
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

    env.write("work/docs/protected/contract.md", "protected change\n");
    git(&["add", "docs/protected/contract.md"]);
    git(&["commit", "-m", "owner-reviewed change"]);
    let reviewed_head = git(&["rev-parse", "HEAD"]);

    for (paths, owner_opinion, outstanding) in [
        (
            &["docs/token-usage.md", "engine/tests/regression.rs"][..],
            "APPROVED",
            serde_json::json!(["codex", "security"]),
        ),
        (&[][..], "APPROVED", serde_json::json!(["codex", "security"])),
        (
            &["docs/token-usage.md"][..],
            "DISMISSED",
            serde_json::json!(["codex", "security", "owner"]),
        ),
        (
            &["docs/AGENTS.md"][..],
            "APPROVED",
            serde_json::json!(["codex", "security", "owner"]),
        ),
        (
            &["docs/protected/guide.md"][..],
            "APPROVED",
            serde_json::json!(["codex", "security", "owner"]),
        ),
        (
            &["engine/other.rs"][..],
            "APPROVED",
            serde_json::json!(["codex", "security", "owner"]),
        ),
    ] {
        git(&["reset", "--hard", &reviewed_head]);
        for path in paths {
            env.write(&format!("work/{path}"), "follow-up\n");
            git(&["add", "--", path]);
        }
        git(&["commit", "--allow-empty", "-m", "follow-up"]);
        let new_head = git(&["rev-parse", "HEAD"]);
        detail["data"]["repository"]["pullRequests"]["nodes"][0]["headRefOid"] = new_head.clone().into();
        let row = &mut detail["data"]["repository"]["p42"];
        row["headRefOid"] = new_head.into();
        row["comments"] = serde_json::json!({"totalCount":0,"nodes":[]});
        row["latestOpinionatedReviews"] = serde_json::json!({
            "totalCount":3,
            "nodes":[
                {"state":"APPROVED","commit":{"oid":reviewed_head},"author":{"login":"code-reviewer"}},
                {"state":"APPROVED","commit":{"oid":reviewed_head},"author":{"login":"security-reviewer"}},
                {"state":owner_opinion,"commit":{"oid":reviewed_head},"author":{"login":"owner-reviewer"}}
            ]
        });
        fs::write(env.work().join("first.json"), detail.to_string()).unwrap();
        fs::write(env.work().join("detail.json"), detail.to_string()).unwrap();
        let out = invoke(
            &env,
            "work board --shadow --repo example/repo --once --review-policy ../home/review.json",
        );
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let log = fs::read_to_string(log_for(&env, "example/repo")).unwrap();
        let sample: serde_json::Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(
            sample["reviews"]["42"]["decision"]["review"]["outstanding"], outstanding,
            "{paths:?}"
        );
    }
}

fn queue_rejection(group: &str) -> serde_json::Value {
    serde_json::json!({"data":{"repository":{
        "mergeQueue":{"id":"queue-1"},
        "p42":{
            "number":42,"headRefOid":"synthetic-head","state":"OPEN","baseRefName":"main",
            "timelineItems":{"nodes":[{
                "__typename":"RemovedFromMergeQueueEvent","id":"removal-1","reason":"failed_checks",
                "createdAt":"2026-01-01T00:00:01Z","mergeQueue":{"id":"queue-1"},
                "beforeCommit":{"oid":group,"statusCheckRollup":{
                    "commit":{"oid":group},"contexts":{"totalCount":1,"nodes":[{
                        "__typename":"CheckRun","databaseId":11,"isRequired":true,
                        "status":"COMPLETED","conclusion":"FAILURE","completedAt":"2026-01-01T00:00:00Z"
                    }]}
                }}
            }]}
        }
    }}})
}

#[test]
fn real_shadow_cli_records_exact_queue_evidence_only_when_opted_in() {
    use serde_json::{json, Value};
    let (env, _) = fixture();
    let group = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let queue = queue_rejection(group);
    fs::write(env.work().join("queue.json"), queue.to_string()).unwrap();

    let args = "work board --shadow --repo example/repo --once";
    assert_eq!(
        invoke(&env, &format!("--offline {args} --queue-evidence"))
            .status
            .code(),
        Some(2)
    );
    assert!(!env.work().join("gh-calls").exists());
    assert!(invoke(&env, args).status.success());
    assert!(!env.work().join("queue-calls").exists());

    let output = invoke(&env, &format!("{args} --queue-evidence"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(env.work().join("queue-calls")).unwrap(),
        "read\n"
    );

    let text = fs::read_to_string(log_for(&env, "example/repo")).unwrap();
    let mut samples = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap());
    assert!(samples.next().unwrap().get("queue_rejections").is_none());
    assert_eq!(
        samples.next().unwrap()["queue_rejections"],
        json!([{
            "number":42, "removal_event":"removal-1","merge_group_head":group,
            "removed_at":"2026-01-01T00:00:01Z",
            "failed_required_check_runs":[11]
        }])
    );
}

#[test]
fn report_policy_records_only_archives_verified_for_all_required_failed_checks() {
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use std::io::{Cursor, Write};
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};
    let (env, mut detail) = fixture();
    fs::rename(env.tmp().join("gh"), env.tmp().join("gh-fixture")).unwrap();
    let gh = env.write(
        "tmp/gh",
        r#"#!/bin/sh
if [ "$1 $2 $3" = 'api --hostname github.com' ]; then
  case "$#:$4:$5:$6:$7" in
    '7:--include:graphql:-f:query=query{'*|'6:--method:GET:repos/example/repo/actions/'*':')
      exec "$0-fixture" "$@" ;;
  esac
fi
printf 'unexpected command\n' >> unexpected-github-command
exit 94
"#,
    );
    fs::set_permissions(gh, fs::Permissions::from_mode(0o700)).unwrap();
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let second_head = "dddddddddddddddddddddddddddddddddddddddd";
    let main = "cccccccccccccccccccccccccccccccccccccccc";
    let group = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let workflow_path = ".github/workflows/checks.yml";
    let mut queue = queue_rejection(group);
    let contexts = queue
        .pointer_mut("/data/repository/p42/timelineItems/nodes/0/beforeCommit/statusCheckRollup/contexts")
        .unwrap();
    contexts["nodes"][0]["checkSuite"] = json!({
        "databaseId":51,"commit":{"oid":group},
        "workflowRun":{"databaseId":61,"event":"merge_group","runAttempt":1}
    });
    let mut second = contexts["nodes"][0].clone();
    second["databaseId"] = 12.into();
    contexts["nodes"].as_array_mut().unwrap().push(second);
    contexts["totalCount"] = 2.into();
    let write = |file: &str, value: Value| {
        fs::write(env.work().join(file), value.to_string()).unwrap();
    };
    let repo = &mut detail["data"]["repository"];
    repo["p42"]["headRefOid"] = head.into();
    repo["pullRequests"]["nodes"][0]["headRefOid"] = head.into();
    let mut second = repo["p42"].clone();
    second["number"] = 43.into();
    second["headRefOid"] = second_head.into();
    repo["p43"] = second;
    repo["pullRequests"]["totalCount"] = 2.into();
    repo["pullRequests"]["nodes"].as_array_mut().unwrap().push(json!({
        "number":43,"headRefOid":second_head
    }));
    write("first.json", detail.clone());
    write("detail.json", detail);
    let rows = &mut queue["data"]["repository"];
    rows["p42"]["headRefOid"] = head.into();
    rows["p42"]["id"] = "pr-42".into();
    rows["p42"]["author"] = json!({"__typename":"User","id":"user-1","login":"sample-author"});
    let mut second = rows["p42"].clone();
    second["number"] = 43.into();
    second["id"] = "pr-43".into();
    second["author"] = json!({"__typename":"Bot","id":"bot-2","login":"sample-agent[bot]"});
    second["headRefOid"] = second_head.into();
    second["timelineItems"]["nodes"][0]["id"] = "removal-2".into();
    rows["p43"] = second;
    let valid_queue = queue.clone();
    write("queue.json", queue.clone());
    write(
        "report-attempt.json",
        json!({"id":61,"run_attempt":1,"check_suite_id":51,"head_sha":group,
        "event":"merge_group","status":"completed","path":workflow_path,"run_started_at":"2025-12-31T23:59:40Z"}),
    );
    write(
        "report-jobs.json",
        json!({"total_count":3,"jobs":[
            {"id":11,"name":"Test aggregate","run_id":61,"run_attempt":1,"head_sha":group,
             "status":"completed","conclusion":"failure","completed_at":"2026-01-01T00:00:00Z"},
            {"id":12,"name":"Gate","run_id":61,"run_attempt":1,"head_sha":group,
             "status":"completed","conclusion":"failure","completed_at":"2026-01-01T00:00:00Z"},
            {"id":21,"name":"Test shard","run_id":61,"run_attempt":1,"head_sha":group,
             "status":"completed","started_at":"2025-12-31T23:59:50Z",
             "completed_at":"2025-12-31T23:59:58Z"}
        ]}),
    );
    let policy = env.write(
        "tmp/report-policy.json",
        &json!({
            "workflow_path":workflow_path,
            "required_check_names":["Test aggregate","Gate"],
            "reports":[{"producer_name":"Test shard","artifact_name":"report-1"}]
        })
        .to_string(),
    );
    fs::set_permissions(&policy, fs::Permissions::from_mode(0o600)).unwrap();

    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    archive
        .start_file(
            "junit.xml",
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .unwrap();
    archive
        .write_all(
            br#"<testsuite name="example">
  <testcase classname="example" name="passed"/>
  <testcase classname="example" name="failed"><failure>synthetic output</failure></testcase>
</testsuite>"#,
        )
        .unwrap();
    let bytes = archive.finish().unwrap().into_inner();
    fs::write(env.work().join("report.zip"), &bytes).unwrap();
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let listing = |sha256: &str| {
        json!({"total_count":1,"artifacts":[{"id":71,"name":"report-1",
        "size_in_bytes":bytes.len(),"digest":format!("sha256:{sha256}"),"expired":false,
        "created_at":"2025-12-31T23:59:57Z","updated_at":"2025-12-31T23:59:57Z",
        "workflow_run":{"id":61,"head_sha":group}}]})
    };
    write("report-artifacts.json", listing(&digest));
    let args = format!(
        "work board --shadow --repo example/repo --once --queue-report-policy {}",
        policy.display()
    );
    let result = invoke(&env, &args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let log = log_for(&env, "example/repo");
    let saved = fs::read_to_string(&log).unwrap();
    let sample: Value = serde_json::from_str(saved.trim()).unwrap();
    assert!(sample.get("landing").is_none());
    for (index, number, event) in [(0, 42, "removal-1"), (1, 43, "removal-2")] {
        let row = &sample["queue_rejections"][index];
        assert_eq!(row["number"], number);
        assert_eq!(row["removal_event"], event);
        assert_eq!(row["merge_group_head"], group);
        assert_eq!(
            row["verified_test_reports"],
            json!([{
                "failed_check_runs":[11,12],"workflow_run":61,"run_attempt":1,
                "archives":[{"id":71,"sha256":digest}],"failed_tests":["example::failed"]
            }])
        );
    }
    for log in ["report-reads", "report-attempt-reads"] {
        assert_eq!(fs::read_to_string(env.work().join(log)).unwrap(), "read\n");
    }

    write("report-artifacts.json", listing(&"0".repeat(64)));
    let result = invoke(&env, &args);
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("GitHub test archive digest mismatch"));
    assert_eq!(fs::read_to_string(&log).unwrap(), saved);

    let job_path = env.work().join("report-jobs.json");
    let mut jobs: Value = serde_json::from_str(&fs::read_to_string(job_path).unwrap()).unwrap();
    let valid_jobs = jobs.clone();
    jobs["jobs"][1]["name"] = "Unrelated check".into();
    write("report-jobs.json", jobs);
    let result = invoke(&env, &args);
    assert!(result.status.success());
    let updated = fs::read_to_string(&log).unwrap();
    assert!(updated.starts_with(&saved));
    let sample: Value = serde_json::from_str(updated.lines().last().unwrap()).unwrap();
    assert!(sample["queue_rejections"][0]
        .get("verified_test_reports")
        .is_none());
    assert_eq!(
        fs::read_to_string(env.work().join("report-reads")).unwrap(),
        "read\nread\n"
    );

    // This REST reply fits in isolation, but the board has already used part of the budget.
    let attempt_path = env.work().join("report-attempt.json");
    let mut attempt: Value = serde_json::from_str(&fs::read_to_string(&attempt_path).unwrap()).unwrap();
    let valid_attempt = attempt.clone();
    let cap = 4 * 1024 * 1024;
    attempt["padding"] = "a".repeat(cap - 512).into();
    assert!(attempt.to_string().len() < cap);
    write("report-attempt.json", attempt);
    let result = invoke(&env, &args);
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("GitHub Actions response exceeds its limit"));
    assert_eq!(fs::read_to_string(&log).unwrap(), updated);

    write("report-jobs.json", valid_jobs);
    write("report-attempt.json", valid_attempt);
    write("report-artifacts.json", listing(&digest));
    let early = &mut queue["data"]["repository"]["p43"]["timelineItems"]["nodes"][0];
    let before_completion = json!("2025-12-31T23:59:59Z");
    early["createdAt"] = before_completion.clone();
    for check in early["beforeCommit"]["statusCheckRollup"]["contexts"]["nodes"]
        .as_array_mut()
        .unwrap()
    {
        check["completedAt"] = before_completion.clone();
    }
    write("queue.json", queue);
    let result = invoke(&env, &args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = fs::read_to_string(&log).unwrap();
    let sample: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(
        sample["queue_rejections"][0]["verified_test_reports"][0]["failed_check_runs"],
        json!([11, 12])
    );
    assert_eq!(sample["queue_rejections"][1]["removal_event"], "removal-2");
    assert!(sample["queue_rejections"][1]
        .get("verified_test_reports")
        .is_none());
    assert_eq!(
        fs::read_to_string(env.work().join("report-reads")).unwrap(),
        "read\nread\nread\n"
    );

    let mut wrong_suite = valid_queue.clone();
    let checks = &mut wrong_suite["data"]["repository"]["p43"]["timelineItems"]["nodes"][0]["beforeCommit"]
        ["statusCheckRollup"]["contexts"]["nodes"];
    for check in checks.as_array_mut().unwrap() {
        check["checkSuite"]["databaseId"] = 52.into();
    }
    write("queue.json", wrong_suite);
    let result = invoke(&env, &args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = fs::read_to_string(log_for(&env, "example/repo")).unwrap();
    let sample: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert_eq!(
        sample["queue_rejections"][0]["verified_test_reports"][0]["archives"],
        json!([{"id":71,"sha256":digest}])
    );
    let other = &sample["queue_rejections"][1];
    assert_eq!(other["merge_group_head"], group);
    assert_eq!(other["removal_event"], "removal-2");
    assert_eq!(other["failed_required_check_runs"], json!([11, 12]));
    assert!(other.get("verified_test_reports").is_none());
    assert_eq!(
        fs::read_to_string(env.work().join("report-reads")).unwrap(),
        "read\nread\nread\nread\n"
    );
    let invalid_archive = vec![0; bytes.len()];
    let invalid_digest = format!("{:x}", Sha256::digest(&invalid_archive));
    fs::write(env.work().join("report.zip"), invalid_archive).unwrap();
    write("report-artifacts.json", listing(&invalid_digest));
    let result = invoke(&env, &args);
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("invalid CI report archive"));
    assert_eq!(fs::read_to_string(log_for(&env, "example/repo")).unwrap(), text);
    fs::write(env.work().join("report.zip"), &bytes).unwrap();
    write("report-artifacts.json", listing(&digest));

    let mut historical: Value = serde_json::from_str(saved.trim()).unwrap();
    historical["observed_at"] = 1_767_225_600.into();
    historical["board"]["observed_at"] = 1_767_225_600.into();
    historical["queue_rejections"] = json!([]);
    let admission = json!({"next":"already_queued","queue_alarm":false,
        "queue":{"enqueued_at":1_767_225_595u64,"group_head":group}});
    historical["landing"] = json!({"42":admission,"43":admission});
    fs::write(&log, format!("{historical}\n")).unwrap();
    write("queue.json", valid_queue);
    let queue_state = |rows: Vec<Value>| {
        json!({"data":{"repository":{
            "main":{"target":{"oid":main}},"mergeQueue":{"entries":{"totalCount":rows.len(),"nodes":rows}}
        }}})
    };
    write("queue-state.json", queue_state(vec![]));
    write(
        "review.json",
        json!({"data":{"repository":{
            "p42":{"number":42,"state":"OPEN","headRefOid":head,"comments":{"totalCount":0,"nodes":[]}},
            "p43":{"number":43,"state":"OPEN","headRefOid":second_head,"comments":{"totalCount":0,"nodes":[]}}
        }}}),
    );
    let shadow_args = format!("{args} --landing-shadow");
    let result = invoke(&env, &shadow_args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let sampled = fs::read_to_string(&log).unwrap();
    let row: Value = serde_json::from_str(sampled.lines().last().unwrap()).unwrap();
    assert_eq!(
        row["landing"]["42"]["would_notify_rejection"],
        json!({
            "pr_head":head,"merge_group_head":group,"removal_event":"removal-1",
            "target":{"pull_request_id":"pr-42","author_id":"user-1","login":"sample-author"},
            "failed_tests":["example::failed"]
        })
    );
    let second_notice = json!({
        "pr_head":second_head,"merge_group_head":group,"removal_event":"removal-2",
        "target":{"pull_request_id":"pr-43","author_id":"bot-2","login":"sample-agent[bot]"},
        "failed_tests":["example::failed"]
    });
    assert_eq!(row["landing"]["43"]["would_notify_rejection"], second_notice);
    for rejection in row["queue_rejections"].as_array().unwrap() {
        assert!(rejection.get("notification_target").is_none());
    }

    let queue_path = env.work().join("queue.json");
    let original: Value = serde_json::from_str(&fs::read_to_string(queue_path).unwrap()).unwrap();
    let mut hidden = original.clone();
    hidden["data"]["repository"]["p42"]["author"] = Value::Null;
    write("queue.json", hidden);
    let result = invoke(&env, &shadow_args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let sampled = fs::read_to_string(&log).unwrap();
    let row: Value = serde_json::from_str(sampled.lines().last().unwrap()).unwrap();
    assert!(row["queue_rejections"][0]["verified_test_reports"].is_array());
    assert!(row["landing"]["42"].get("would_notify_rejection").is_none());
    assert_eq!(row["landing"]["43"]["would_notify_rejection"], second_notice);
    write("queue.json", original);

    let current_row = |time| json!({"pullRequest":{"number":42,"headRefOid":head},"enqueuedAt":time});
    write(
        "queue-state.json",
        queue_state(vec![current_row(json!("2026-01-01T00:00:00Z"))]),
    );
    let result = invoke(&env, &shadow_args);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let sampled = fs::read_to_string(&log).unwrap();
    let row: Value = serde_json::from_str(sampled.lines().last().unwrap()).unwrap();
    assert!(row["landing"]["42"]["queue"].is_object());
    assert!(row["landing"]["42"].get("would_notify_rejection").is_none());

    write("queue-state.json", queue_state(vec![current_row(Value::Null)]));
    let result = invoke(&env, &shadow_args);
    assert_eq!(result.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&result.stderr).contains("invalid enqueue time"));
    assert_eq!(fs::read_to_string(log).unwrap(), sampled);
    assert!(!env.work().join("unexpected-github-command").exists());
}

#[test]
fn landing_shadow_holds_an_approved_head_on_unresolved_current_or_older_reviews() {
    use serde_json::{json, Value};
    let (env, mut detail) = fixture();
    let head = "cccccccccccccccccccccccccccccccccccccccc";
    let main = "dddddddddddddddddddddddddddddddddddddddd";
    let group = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let row = &mut detail["data"]["repository"]["p42"];
    row["headRefOid"] = head.into();
    row["mergeable"] = "MERGEABLE".into();
    row["mergeStateStatus"] = "CLEAN".into();
    row["reviewDecision"] = "APPROVED".into();
    row["latestOpinionatedReviews"] =
        json!({"totalCount":1,"nodes":[{"state":"APPROVED","commit":{"oid":head}}]});
    row["commits"] = json!({"nodes":[{"commit":{"statusCheckRollup":{"contexts":{
        "totalCount":1,"nodes":[{"name":"ci","isRequired":true,"status":"COMPLETED","conclusion":"SUCCESS"}]
    }}}}]});
    detail["data"]["repository"]["pullRequests"]["nodes"][0]["headRefOid"] = head.into();
    fs::write(env.work().join("first.json"), detail.to_string()).unwrap();
    assert_eq!(sample(&env, &detail)["board"]["entries"][0]["action"], "ready");
    fs::write(
        env.work().join("queue.json"),
        json!({"data":{"repository":{"mergeQueue":null}}}).to_string(),
    )
    .unwrap();
    let mut queue = json!({"data":{"repository":{
        "main":{"target":{"oid":main}},
        "mergeQueue":{"entries":{"totalCount":1,"nodes":[{
            "enqueuedAt":"2020-01-01T00:00:00Z",
            "headCommit":{"oid":group},
            "pullRequest":{"number":42,"headRefOid":head}
        }]}}
    }}});
    let save_queue = |queue: &Value| {
        fs::write(env.work().join("queue-state.json"), queue.to_string()).unwrap();
    };
    let old = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let completed = |commit, unresolved| {
        json!({
            "commit_sha":commit,"status":"completed",
            "has_any_findings":unresolved,"has_unresolved_findings":unresolved
        })
    };
    let summary = |code, security| {
        let state = json!({"code_review":code,"security_review":security});
        format!("<!-- codex-pull-request-review-summary -->\n<!-- codex-review-state:v1 {state} -->")
    };
    let mut review = json!({"data":{"repository":{"p42":{
        "id":"PR_fixture","number":42,"headRefOid":head,"state":"OPEN",
        "autoMergeRequest":{"enabledAt":"2026-01-01T00:00:00Z"},"mergeQueueEntry":null,
        "comments":{"totalCount":1,"nodes":[{
            "author":{"databaseId":199175422},"updatedAt":"2026-01-01T00:00:00Z",
            "body":summary(completed(head, false), completed(head, false))
        }]}
    }}}});
    let read = |review: &Value| -> Value {
        fs::write(env.work().join("review.json"), review.to_string()).unwrap();
        let output = invoke(
            &env,
            "work board --shadow --repo example/repo --once --landing-shadow",
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = fs::read_to_string(log_for(&env, "example/repo")).unwrap();
        let data: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(data["board"]["entries"][0]["action"], "ready");
        data
    };
    let mut absent = queue.clone();
    absent["data"]["repository"]["mergeQueue"]["entries"] = json!({"totalCount":0,"nodes":[]});
    save_queue(&absent);
    let mut missing_code = review.clone();
    missing_code["data"]["repository"]["p42"]["comments"] = json!({"totalCount":0,"nodes":[]});
    assert_eq!(read(&missing_code)["landing"]["42"]["next"], "wait_review");

    let mut pending = completed(head, false);
    pending["status"] = "in_progress".into();
    for code in [pending, completed(old, false)] {
        let mut without_exact_clearance = review.clone();
        without_exact_clearance["data"]["repository"]["p42"]["comments"]["nodes"][0]["body"] =
            summary(code, completed(head, false)).into();
        let decision = read(&without_exact_clearance);
        assert_eq!(decision["landing"]["42"]["next"], "wait_review");
        assert!(decision["landing"]["42"].get("hold_intervention").is_none());
    }
    assert_eq!(read(&review)["landing"]["42"]["next"], "wait_queue");
    save_queue(&queue);
    review["data"]["repository"]["p42"]["mergeQueueEntry"] = json!({"id":"MQE_fixture"});
    let admitted = read(&review);
    assert_eq!(admitted["landing"]["42"]["next"], "already_queued");
    assert_eq!(admitted["landing"]["42"]["queue"]["group_head"], group);
    assert!(admitted["landing"]["42"].get("hold_intervention").is_none());

    let comment = &mut review["data"]["repository"]["p42"]["comments"]["nodes"][0];
    comment["updatedAt"] = "2026-01-01T00:00:01Z".into();
    comment["body"] = summary(completed(head, true), completed(head, false)).into();
    let current = read(&review);
    assert_eq!(current["landing"]["42"]["next"], "hold_findings");
    assert_eq!(current["landing"]["42"]["queue"]["group_head"], group);
    assert_eq!(
        current["landing"]["42"]["hold_intervention"],
        json!({
            "pull_request_id":"PR_fixture","head":head,"disable_auto_merge":true,"dequeue":true,
        })
    );
    let mut separate_security = review.clone();
    separate_security["data"]["repository"]["p42"]["comments"] = json!({
        "totalCount":2,"nodes":[{
            "author":{"databaseId":199175422},"updatedAt":"2026-01-01T00:00:01Z",
            "body":summary(completed(head, true), json!({
                "commit_sha":head,"status":"completed",
                "has_any_findings":null,"has_unresolved_findings":null
            }))
        },{
            "author":{"databaseId":199175422},"updatedAt":"2026-01-01T00:00:02Z",
            "body":"<!-- codex-review-receipt:v1 -->\nSecurity review completed with no findings."
        }]
    });
    assert_eq!(
        read(&separate_security)["landing"]["42"],
        current["landing"]["42"]
    );
    let mut incomplete = review.clone();
    let mut code = completed(head, true);
    code["status"] = "in_progress".into();
    incomplete["data"]["repository"]["p42"]["comments"]["nodes"][0]["body"] =
        summary(code, completed(head, false)).into();
    let incomplete = read(&incomplete);
    assert_eq!(incomplete["landing"]["42"]["next"], "hold_findings");
    assert!(incomplete["landing"]["42"].get("hold_intervention").is_none());
    let mut no_auto_merge = review.clone();
    no_auto_merge["data"]["repository"]["p42"]["autoMergeRequest"] = Value::Null;
    assert_eq!(
        read(&no_auto_merge)["landing"]["42"]["hold_intervention"],
        json!({
            "pull_request_id":"PR_fixture","head":head,"disable_auto_merge":false,"dequeue":true,
        })
    );
    save_queue(&absent);
    review["data"]["repository"]["p42"]["mergeQueueEntry"] = Value::Null;
    let held_without_entry = read(&review);
    assert_eq!(held_without_entry["landing"]["42"]["next"], "hold_findings");
    assert_eq!(
        held_without_entry["landing"]["42"]["hold_intervention"],
        json!({
            "pull_request_id":"PR_fixture","head":head,"disable_auto_merge":true,"dequeue":false,
        })
    );
    review["data"]["repository"]["p42"]["autoMergeRequest"] = Value::Null;
    assert!(read(&review)["landing"]["42"].get("hold_intervention").is_none());
    save_queue(&queue);
    review["data"]["repository"]["p42"]["mergeQueueEntry"] = json!({"id":"MQE_fixture"});

    review["data"]["repository"]["p42"]["comments"]["nodes"][0]["body"] = summary(
        completed(old, true),
        json!({"commit_sha":null,"status":null,"has_any_findings":null,"has_unresolved_findings":null}),
    )
    .into();
    let observed = read(&review);
    let hold = &observed["landing"]["42"];
    assert_eq!(hold["next"], "hold_findings");
    assert!(hold.get("hold_intervention").is_none());
    assert_eq!(hold["queue_alarm"], true);
    assert_eq!(observed["landing_main"], main);
    assert_eq!(hold["queue"]["group_head"], group);
    review["data"]["repository"]["p42"]["comments"]["totalCount"] = 2.into();
    assert_eq!(read(&review)["landing"]["42"]["next"], "wait_review");
    queue["data"]["repository"]["main"] = Value::Null;
    queue["data"]["repository"]["mergeQueue"]["entries"]["nodes"][0]["headCommit"] = Value::Null;
    save_queue(&queue);
    let waiting = read(&review);
    let waiting_for_group = &waiting["landing"]["42"];
    assert_eq!(waiting_for_group["queue_alarm"], true);
    assert!(waiting_for_group["queue"].get("group_head").is_none());
    assert!(waiting.get("landing_main").is_none());
    queue["data"]["repository"]["mergeQueue"]["entries"]["nodes"][0]["pullRequest"]["headRefOid"] =
        old.into();
    save_queue(&queue);
    let stale = read(&review);
    assert_eq!(stale["landing"]["42"]["queue_alarm"], false);
    assert!(stale["landing"]["42"].get("queue").is_none());
    queue["data"]["repository"]["main"]["target"]["oid"] = main.into();
    queue["data"]["repository"]["mergeQueue"]["entries"] = json!({"totalCount":0,"nodes":[]});
    save_queue(&queue);
    let empty = read(&review);
    assert_eq!(empty["landing_main"], main);
    assert!(empty["landing"]["42"].get("queue").is_none());
}

#[test]
fn landing_shadow_degrades_an_incomplete_queue_connection() {
    let (env, mut detail) = fixture();
    detail["data"]["repository"]["p42"]["comments"] = serde_json::json!({"totalCount": 0, "nodes": []});
    fs::write(env.work().join("review.json"), detail.to_string()).unwrap();
    env.write(
        "work/queue.json",
        r#"{"data":{"repository":{"mergeQueue":null}}}"#,
    );
    let nodes: Vec<_> = (1..=100)
        .map(|number| {
            serde_json::json!({
                "pullRequest": {"number": number, "headRefOid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                "enqueuedAt": "2026-01-01T00:00:00Z"
            })
        })
        .collect();
    let partial = serde_json::json!({"data":{"repository":{
        "mergeQueue":{"entries":{"totalCount":101,"nodes":nodes}}
    }}});
    fs::write(env.work().join("queue-state.json"), partial.to_string()).unwrap();

    let result = invoke(
        &env,
        "work board --shadow --repo example/repo --once --landing-shadow",
    );
    // The board is published; the observed queue rows stay usable and the limit is logged.
    assert_eq!(result.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("merge queue connection exceeds its 100-item page (101 entries)"),
        "unexpected result: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(fs::metadata(log_for(&env, "example/repo")).unwrap().len() > 0);
}
