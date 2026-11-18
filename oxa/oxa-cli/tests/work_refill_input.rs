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
    let env = TestEnv::new();
    fs::set_permissions(env.tmp(), fs::Permissions::from_mode(0o700)).unwrap();
    let gh = env.write(
        "tmp/gh",
        r#"#!/bin/sh
[ "$GH_TOKEN" = synthetic ] || exit 93
printf 'HTTP/2.0 200 OK\r\n\r\n'
case "$*" in
  *headRefName*) case "$*" in *after:*) cat "$0.second";; *) cat "$0.first";; esac ;;
  *':pullRequest('*) cat "$0.details" ;;
  *) cat "$0.list" ;;
esac
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
        repository[format!("p{n}")] = json!({"number":n,"headRefOid":format!("head-{n}"),
            "baseRefName":"main","state":"OPEN","body":"","isDraft":false,"reviewDecision":null,
            "mergeable":"MERGEABLE","mergeStateStatus":"CLEAN",
            "baseRef":{"refUpdateRule":null,"rules":{"totalCount":0,"nodes":[]}},
            "labels":{"totalCount":0,"nodes":[]},"files":{"totalCount":0,"nodes":[]},
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
