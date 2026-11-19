use oxa_cli::{
    refill_input::{Input, Prepared},
    work_refill::Source,
};
use oxa_fake::TestEnv;
use oxa_work::board::{Action, Board, ExistingWork, GitHub, Policy};
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt};

fn write(env: &TestEnv, name: &str, value: &Value) {
    fs::write(env.tmp().join(name), value.to_string()).unwrap();
}

fn fixture(count: u64, linked: bool) -> (TestEnv, Vec<Prepared>) {
    board_fixture(count, linked, BTreeMap::new())
}

/// The same harness with changed-file tails: each entry keeps 100 files in the
/// start page and offers the rest through one bounded continuation.
fn board_fixture(count: u64, linked: bool, tails: BTreeMap<u64, Vec<Value>>) -> (TestEnv, Vec<Prepared>) {
    let env = TestEnv::new();
    fs::set_permissions(env.tmp(), fs::Permissions::from_mode(0o700)).unwrap();
    let gh = env.write(
        "tmp/gh",
        r#"#!/bin/sh
[ "$GH_TOKEN" = synthetic ] || exit 93
reply=
case "$*" in
  *'files(first:100,after:'*)
    case "$*" in
      *'p100:pullRequest('*) reply="$0.files100" ;;
      *'p101:pullRequest('*) reply="$0.files101" ;;
      *) exit 8 ;;
    esac ;;
  *headRefName*) case "$*" in *after:*) reply="$0.second";; *) reply="$0.first";; esac ;;
  *':pullRequest('*) reply="$0.details" ;;
  *) reply="$0.list" ;;
esac
status=200
[ -f "$reply.status" ] && status=$(cat "$reply.status")
printf 'HTTP/2.0 %s Synthetic\r\n\r\n' "$status"
[ "$status" = 200 ] || exit 0
exec /bin/cat "$reply"
"#,
    );
    fs::set_permissions(gh, fs::Permissions::from_mode(0o700)).unwrap();
    let list: Vec<_> = (100..100 + count)
        .map(|n| json!({"number":n,"headRefOid":format!("head-{n}")}))
        .collect();
    let mut repository = json!({"mergeQueue":null,"pullRequests":{
        "totalCount":count,"nodes":list,"pageInfo":{"hasNextPage":false,"endCursor":"last"}}});
    write(&env, "gh.list", &json!({"data":{"repository":repository}}));
    for n in 100..100 + count {
        let files = match tails.get(&n) {
            None => json!({"totalCount":0,"nodes":[],
                "pageInfo":{"hasNextPage":false,"endCursor":null}}),
            Some(tail) => {
                let head: Vec<Value> = (0..100)
                    .map(|i| json!({"path":format!("src/f{i}.rs"),"deletions":0}))
                    .collect();
                let total = (head.len() + tail.len()) as u64;
                let mut next = json!({"data":{"repository":{}}});
                next["data"]["repository"][format!("p{n}")] = json!({
                    "number":n,"headRefOid":format!("head-{n}"),
                    "files":{"totalCount":total,
                        "pageInfo":{"hasNextPage":false,"endCursor":null},
                        "nodes":tail}});
                write(&env, &format!("gh.files{n}"), &next);
                json!({"totalCount":total,
                    "pageInfo":{"hasNextPage":true,"endCursor":"next-100"},"nodes":head})
            }
        };
        repository[format!("p{n}")] = json!({"number":n,"headRefOid":format!("head-{n}"),
            "baseRefName":"main","state":"OPEN","body":"","isDraft":false,"reviewDecision":null,
            "mergeable":"MERGEABLE","mergeStateStatus":"CLEAN",
            "baseRef":{"refUpdateRule":null,"rules":{"totalCount":0,"nodes":[]}},
            "labels":{"totalCount":0,"nodes":[]},"files":files,
            "reviewThreads":{"totalCount":0,"nodes":[]},"latestOpinionatedReviews":{"totalCount":0,"nodes":[]},
            "commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}});
    }
    write(&env, "gh.details", &json!({"data":{"repository":repository}}));
    let metadata: Vec<_> = (100..100 + count)
        .map(|n| {
            json!({"number":n,
        "headRefOid":format!("head-{n}"),"headRefName":format!("agent/{n}"),"author":{"login":"worker-a[bot]"},
        "closingIssuesReferences":if linked && n == 99+count {
            json!({"totalCount":1,"nodes":[{"number":44}]})
        } else { json!({"totalCount":0,"nodes":[]}) }})
        })
        .collect();
    let split = metadata.len() / 2;
    for (name, rows, more) in [
        ("gh.first", &metadata[..split], true),
        ("gh.second", &metadata[split..], false),
    ] {
        write(
            &env,
            name,
            &json!({"data":{"repository":{
            "pullRequests":{"totalCount":count,"nodes":rows,"pageInfo":{"hasNextPage":more,"endCursor":"next"}},
            "i44":{"number":44,"state":"OPEN","updatedAt":"revision"},"b44":null}}}),
        );
    }
    let packets = vec![Prepared {
        number: 44,
        revision: "revision".into(),
        action: Action::BuildIssue,
        branch: "agent/44".into(),
        task: "i44 local/fake agent/44 main\nSynthetic packet.\n".into(),
    }];
    (env, packets)
}

#[test]
fn all_metadata_pages_preserve_linked_issue_refusal_and_explicit_intake() {
    let (env, packets) = fixture(2, true);
    let gh = env.tmp().join("gh");
    let path = std::env::var_os("PATH").unwrap();
    let mut source = Input::new(&gh, &path, "example/project", "synthetic", &packets, true);
    let snapshot = source.snapshot().unwrap();
    assert_eq!(snapshot.pull_requests.len(), 2);
    assert!(snapshot.issues[0].approved);
    assert!(snapshot.issues[0].has_open_pull_request);
    let mut hosted =
        Input::new(&gh, &path, "example/project", "synthetic", &packets, true).with_host("host-a");
    let policy = Policy {
        enabled: true,
        ..Policy::default()
    };
    let mut entry = Board::read(&mut source, &policy, &BTreeMap::new(), 10)
        .unwrap()
        .entries
        .into_iter()
        .find(|row| row.number == 44)
        .unwrap();
    entry.action = Action::BuildIssue;
    assert!(
        matches!(hosted.existing_work(&entry, None).unwrap(), Some(ExistingWork::PullRequest { number: 101, owner }) if owner == "worker-a[bot]")
    );
    let mut off = Input::new(&gh, &path, "example/project", "synthetic", &packets, false);
    assert!(!off.snapshot().unwrap().issues[0].approved);
    let mut second: Value = serde_json::from_slice(&fs::read(env.tmp().join("gh.second")).unwrap()).unwrap();
    second["data"]["repository"]["pullRequests"]["nodes"][0]["headRefOid"] = "changed-head".into();
    write(&env, "gh.second", &second);
    assert!(source.snapshot().is_err());
}

#[test]
fn fresh_revision_and_global_open_pr_cap_gate_issue_admission() {
    let (env, packets) = fixture(2, false);
    let gh = env.tmp().join("gh");
    let path = std::env::var_os("PATH").unwrap();
    let mut source = Input::new(&gh, &path, "example/project", "synthetic", &packets, true);
    let board = Board::read(
        &mut source,
        &Policy {
            enabled: true,
            ..Policy::default()
        },
        &BTreeMap::new(),
        10,
    )
    .unwrap();
    let entry = board.entries.iter().find(|row| row.number == 44).unwrap();
    assert_eq!(entry.action, Action::BuildIssue);
    assert!(source.revalidate(entry).unwrap());
    let mut first: Value = serde_json::from_slice(&fs::read(env.tmp().join("gh.first")).unwrap()).unwrap();
    first["data"]["repository"]["i44"]["updatedAt"] = "changed-revision".into();
    write(&env, "gh.first", &first);
    assert!(!source.revalidate(entry).unwrap());
    let (capped, packets) = fixture(40, false);
    let gh = capped.tmp().join("gh");
    let mut source = Input::new(&gh, &path, "example/project", "synthetic", &packets, true);
    let snapshot = source.snapshot().unwrap();
    assert_eq!(snapshot.pull_requests.len(), 40);
    assert!(!snapshot.issues[0].approved);
}

fn source_removal_tail() -> Vec<Value> {
    vec![
        json!({"path":"docs/release-note.md","deletions":0}),
        json!({"path":"engine/src/legacy-parser.c","deletions":12}),
    ]
}

fn long_source_removal_tail() -> Vec<Value> {
    let mut tail: Vec<Value> = (0..19)
        .map(|i| json!({"path":format!("docs/tail-{i}.md"),"deletions":0}))
        .collect();
    tail.push(json!({"path":"engine/src/legacy-kernel.hpp","deletions":3}));
    tail
}

fn snapshot_of(env: &TestEnv, packets: &[Prepared]) -> Result<oxa_work::board::Snapshot, String> {
    let gh = env.tmp().join("gh");
    let path = std::env::var_os("PATH").unwrap();
    Input::new(&gh, &path, "example/project", "synthetic", packets, true).snapshot()
}

#[test]
fn changed_file_continuation_completes_a_two_file_tail() {
    let (env, packets) = board_fixture(2, false, BTreeMap::from([(100, source_removal_tail())]));
    let snapshot = snapshot_of(&env, &packets).unwrap();
    assert_eq!(snapshot.pull_requests.len(), 2);
    let short = snapshot.pull_requests.iter().find(|pr| pr.number == 100).unwrap();
    assert_eq!(short.head, "head-100");
    assert!(
        short.removes_source,
        "the tail page's source deletion is evidence the start page cannot see"
    );
    let untouched = snapshot.pull_requests.iter().find(|pr| pr.number == 101).unwrap();
    assert!(!untouched.removes_source);

    // The completed evidence also admits the Board instead of refusing it.
    let gh = env.tmp().join("gh");
    let path = std::env::var_os("PATH").unwrap();
    let mut source = Input::new(&gh, &path, "example/project", "synthetic", &packets, true);
    let board = Board::read(
        &mut source,
        &Policy {
            enabled: true,
            ..Policy::default()
        },
        &BTreeMap::new(),
        10,
    )
    .unwrap();
    let entry = board.entries.iter().find(|row| row.number == 100).unwrap();
    assert_eq!(
        entry.action,
        Action::AddRemoves,
        "the tail page's source deletion must reach the board action"
    );
}

#[test]
fn changed_file_continuation_completes_a_twenty_file_tail() {
    let (env, packets) = board_fixture(2, false, BTreeMap::from([(101, long_source_removal_tail())]));
    let snapshot = snapshot_of(&env, &packets).unwrap();
    assert_eq!(snapshot.pull_requests.len(), 2);
    let long = snapshot.pull_requests.iter().find(|pr| pr.number == 101).unwrap();
    assert_eq!(long.head, "head-101");
    assert!(
        long.removes_source,
        "a source deletion after the first hundred files is still evidence"
    );
    assert!(
        !snapshot
            .pull_requests
            .iter()
            .find(|pr| pr.number == 100)
            .unwrap()
            .removes_source
    );
}

fn continuation_of(env: &TestEnv, number: u64) -> Value {
    serde_json::from_slice(&fs::read(env.tmp().join(format!("gh.files{number}"))).unwrap()).unwrap()
}

#[test]
fn changed_file_continuation_for_a_stale_head_fails_atomically() {
    let (env, packets) = board_fixture(2, false, BTreeMap::from([(100, source_removal_tail())]));
    let mut next = continuation_of(&env, 100);
    next["data"]["repository"]["p100"]["headRefOid"] = "changed-head".into();
    write(&env, "gh.files100", &next);
    let error = snapshot_of(&env, &packets).unwrap_err();
    assert!(error.contains("mismatched PR details"), "{error}");
}

#[test]
fn changed_file_continuation_with_a_changed_total_fails_atomically() {
    let (env, packets) = board_fixture(2, false, BTreeMap::from([(100, source_removal_tail())]));
    let mut next = continuation_of(&env, 100);
    next["data"]["repository"]["p100"]["files"]["totalCount"] = json!(101);
    write(&env, "gh.files100", &next);
    let error = snapshot_of(&env, &packets).unwrap_err();
    assert!(error.contains("mismatched PR details"), "{error}");
}

#[test]
fn changed_file_continuation_without_full_coverage_fails_atomically() {
    let (env, packets) = board_fixture(2, false, BTreeMap::from([(100, source_removal_tail())]));
    let mut next = continuation_of(&env, 100);
    next["data"]["repository"]["p100"]["files"]["nodes"] =
        json!([{"path":"docs/release-note.md","deletions":0}]);
    write(&env, "gh.files100", &next);
    let error = snapshot_of(&env, &packets).unwrap_err();
    assert!(error.contains("incomplete changed-file listing"), "{error}");
}

#[test]
fn changed_file_continuation_repeating_a_path_fails_atomically() {
    let (env, packets) = board_fixture(2, false, BTreeMap::from([(100, source_removal_tail())]));
    let mut next = continuation_of(&env, 100);
    next["data"]["repository"]["p100"]["files"]["nodes"][0] = json!({"path":"src/f0.rs","deletions":4});
    write(&env, "gh.files100", &next);
    let error = snapshot_of(&env, &packets).unwrap_err();
    assert!(error.contains("duplicate changed file"), "{error}");
}

#[test]
fn interrupted_changed_file_continuation_fails_atomically() {
    let (env, packets) = board_fixture(2, false, BTreeMap::from([(100, source_removal_tail())]));
    fs::write(env.tmp().join("gh.files100.status"), "502").unwrap();
    let error = snapshot_of(&env, &packets).unwrap_err();
    assert!(error.contains("GitHub read failed (HTTP 502)"), "{error}");
}
