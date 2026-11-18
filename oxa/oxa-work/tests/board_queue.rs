use oxa_work::{
    board::{queue, Action, Board, Entry},
    shadow,
};
use serde_json::{json, Value};
use std::{fs, time::SystemTime};

#[path = "support/private_dir.rs"]
mod private_dir;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const GROUP: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn board() -> Board {
    Board {
        observed_at: 12,
        entries: vec![Entry {
            number: 42,
            head: HEAD.into(),
            release_pinned: false,
            action: Action::Ready,
            lease: None,
            attempts_today: 0,
            eligible: true,
        }],
    }
}

fn check(id: u64, required: bool, status: &str, conclusion: &str, at: &str) -> Value {
    json!({
        "__typename":"CheckRun", "databaseId":id, "isRequired":required,
        "status":status, "conclusion":conclusion, "completedAt":at
    })
}

fn fixture(conclusion: &str) -> Value {
    let before = "2026-01-01T00:00:00Z";
    let checks = [
        check(15, true, "COMPLETED", conclusion, before),
        check(11, true, "COMPLETED", conclusion, before),
        check(20, false, "COMPLETED", conclusion, before),
        check(21, true, "COMPLETED", "SUCCESS", before),
        check(22, true, "IN_PROGRESS", conclusion, before),
        check(23, true, "COMPLETED", conclusion, "2026-01-01T00:00:02Z"),
        json!({"__typename":"StatusContext","id":24,"isRequired":true,"state":"FAILURE"}),
    ];
    json!({
        "mergeQueue":{"id":"queue-1"},
        "p42":{
            "number":42,"headRefOid":HEAD,"state":"OPEN","baseRefName":"main",
            "timelineItems":{"nodes":[{
                "__typename":"RemovedFromMergeQueueEvent","id":"removal-1","reason":"failed_checks",
                "createdAt":"2026-01-01T00:00:01Z","mergeQueue":{"id":"queue-1"},
                "beforeCommit":{"oid":GROUP,"statusCheckRollup":{
                    "commit":{"oid":GROUP},"contexts":{"totalCount":checks.len(),"nodes":checks}
                }}
            }]}
        }
    })
}

#[test]
fn journals_only_required_failed_runs_of_the_removed_group_in_the_board_sample() {
    for conclusion in ["FAILURE", "TIMED_OUT"] {
        assert_rejection_journal(conclusion);
    }
}

fn assert_rejection_journal(conclusion: &str) {
    let board = board();
    let evidence = queue::read(&board, |fields| {
        assert!(fields.contains("p42:pullRequest(number:42)"));
        assert!(fields.contains("isRequired(pullRequestNumber:42)"));
        assert!(fields.contains("beforeCommit{oid statusCheckRollup"));
        Ok((fixture(conclusion), 4096))
    })
    .unwrap();
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("oxa-queue-shadow-{}-{nonce}", std::process::id()));
    let mut log = shadow::open_log(&path).unwrap();
    shadow::run_with_queue_evidence(
        &mut log,
        true,
        || Ok((board.clone(), evidence.clone())),
        |_| unreachable!(),
    )
    .unwrap();
    shadow::run(&mut log, true, || Ok(board.clone()), |_| unreachable!()).unwrap();
    drop(log);
    let text = fs::read_to_string(&path).unwrap();
    fs::remove_file(path).unwrap();
    let mut samples = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap());
    let sample = samples.next().unwrap();
    assert_eq!(sample["board"]["entries"][0]["head"], HEAD);
    assert_eq!(
        sample["queue_rejections"],
        json!([{
            "number":42, "removal_event":"removal-1", "merge_group_head":GROUP,
            "removed_at":"2026-01-01T00:00:01Z",
            "failed_required_check_runs":[11,15]
        }])
    );
    assert!(samples.next().unwrap().get("queue_rejections").is_none());
}

#[test]
fn absent_or_unverified_evidence_never_creates_a_rejection() {
    let board = board();
    let path = "/p42/timelineItems/nodes/0";
    for (field, replacement) in [
        ("", json!({"__typename":"AddedToMergeQueueEvent"})),
        ("/reason", json!("main failed checks")),
        ("/reason", json!("merge_conflict")),
        ("/mergeQueue/id", json!("different-queue")),
        ("/beforeCommit", Value::Null),
        ("/beforeCommit/oid", json!(HEAD)),
        ("/beforeCommit/statusCheckRollup/commit/oid", json!(HEAD)),
        ("/beforeCommit/statusCheckRollup/contexts/totalCount", json!(101)),
        ("/createdAt", json!("2025-01-01T00:00:00Z")),
    ] {
        let mut data = fixture("FAILURE");
        *data.pointer_mut(&format!("{path}{field}")).unwrap() = replacement;
        assert!(
            queue::read(&board, |_| Ok((data.clone(), 4096)))
                .unwrap()
                .is_empty(),
            "{field}"
        );
    }
}

#[test]
fn impossible_utc_times_do_not_establish_rejection_evidence() {
    let path = "/p42/timelineItems/nodes/0";
    let mut invalid_removal = fixture("FAILURE");
    invalid_removal.pointer_mut(path).unwrap()["createdAt"] = json!("2026-04-31T00:00:01Z");

    let mut partly_invalid_checks = fixture("FAILURE");
    let runs = partly_invalid_checks
        .pointer_mut(&format!("{path}/beforeCommit/statusCheckRollup/contexts/nodes"))
        .unwrap()
        .as_array_mut()
        .unwrap();
    runs[0]["completedAt"] = json!("2025-02-29T00:00:00Z");

    for inputs in [invalid_removal, partly_invalid_checks] {
        let evidence = queue::read(&board(), |_| Ok((inputs.clone(), 4096))).unwrap();
        assert!(evidence.is_empty());
    }
}

#[test]
fn changed_pr_head_refuses_the_snapshot() {
    let mut data = fixture("FAILURE");
    data["p42"]["headRefOid"] = json!(GROUP);
    assert!(queue::read(&board(), |_| Ok((data.clone(), 4096))).is_err());
}

#[test]
fn the_owner_journal_matches_only_the_previously_queued_head_and_group() {
    use oxa_work::board::landing::{LatestReviews, Next};
    use oxa_work::board::queue::report::{VerifiedArchive, VerifiedReports};
    use shadow::{LandingDecision, LandingSample, QueueObservation};
    use std::collections::BTreeMap;

    let path = std::env::temp_dir().join(format!(
        "oxa-queue-admission-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut prior = board();
    prior.observed_at = 1_767_225_600;
    for observed_at in [prior.observed_at, prior.observed_at + 1] {
        let mut log = shadow::open_log(&path).unwrap();
        shadow::run_with_landing(
            &mut log,
            true,
            || {
                let mut queued_board = prior.clone();
                queued_board.observed_at = observed_at;
                Ok(LandingSample {
                    board: queued_board,
                    landing: BTreeMap::from([(
                        42,
                        LandingDecision {
                            next: Next::AlreadyQueued,
                            queue_alarm: false,
                            repeat_rejection_alarm: false,
                            latest_reviews: LatestReviews::Clear,
                            would_notify_rejection: None,
                            queue: Some(QueueObservation {
                                enqueued_at: prior.observed_at - 5,
                                group_head: Some(GROUP.into()),
                            }),
                        },
                    )]),
                    ..Default::default()
                })
            },
            |_| unreachable!(),
        )
        .unwrap();
    }

    let mut current = prior.clone();
    current.observed_at += 3;
    let other = "cccccccccccccccccccccccccccccccccccccccc";
    let first_main = "dddddddddddddddddddddddddddddddddddddddd";
    let later_main = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    for (index, (head, group, main, report_ids, reviews)) in [
        (HEAD, GROUP, first_main, &[][..], LatestReviews::Clear),
        (HEAD, GROUP, later_main, &[11][..], LatestReviews::Clear),
        (HEAD, GROUP, later_main, &[11, 15][..], LatestReviews::Clear),
        (HEAD, GROUP, later_main, &[11, 15][..], LatestReviews::Unresolved),
        (HEAD, other, later_main, &[11, 15][..], LatestReviews::Clear),
        (HEAD, GROUP, later_main, &[11, 15][..], LatestReviews::Clear),
        (HEAD, other, later_main, &[11, 15][..], LatestReviews::Clear),
        (other, GROUP, later_main, &[11, 15][..], LatestReviews::Clear),
    ]
    .into_iter()
    .enumerate()
    {
        let mut log = shadow::open_log(&path).unwrap();
        current.entries[0].head = head.into();
        let mut evidence = fixture("FAILURE");
        evidence["p42"]["id"] = "pr-42".into();
        evidence["p42"]["author"] = json!({"__typename":"User","id":"user-1","login":"sample-author"});
        evidence["p42"]["headRefOid"] = head.into();
        let group_event = &mut evidence["p42"]["timelineItems"]["nodes"][0]["beforeCommit"];
        group_event["oid"] = group.into();
        group_event["statusCheckRollup"]["commit"]["oid"] = group.into();
        for check in group_event["statusCheckRollup"]["contexts"]["nodes"]
            .as_array_mut()
            .unwrap()
        {
            check["checkSuite"] = json!({
                "databaseId": 51, "commit": {"oid": group},
                "workflowRun": {"databaseId": 61, "event": "merge_group", "runAttempt": 1}
            });
        }
        let mut rejections = queue::read(&current, |_| Ok((evidence.clone(), 4096))).unwrap();
        if !report_ids.is_empty() {
            rejections[0].record_verified_reports(VerifiedReports {
                failed_check_runs: report_ids.to_vec(),
                workflow_run: 61,
                run_attempt: 1,
                archives: vec![VerifiedArchive {
                    id: 71,
                    sha256: "a".repeat(64),
                }],
                failed_tests: std::iter::once("example::failed".into())
                    .chain(report_ids.iter().map(|id| format!("example::failed_{id}")))
                    .collect(),
            });
        }
        shadow::run_with_landing(
            &mut log,
            true,
            || {
                let queued = (index == 5).then(|| QueueObservation {
                    enqueued_at: current.observed_at,
                    group_head: Some(other.into()),
                });
                Ok(LandingSample {
                    board: current.clone(),
                    landing_main: Some(main.into()),
                    queue_rejections: rejections.clone(),
                    landing: BTreeMap::from([(
                        42,
                        LandingDecision {
                            next: if queued.is_some() {
                                Next::AlreadyQueued
                            } else if reviews == LatestReviews::Unresolved {
                                Next::HoldFindings
                            } else {
                                Next::WaitQueue
                            },
                            queue_alarm: false,
                            repeat_rejection_alarm: false,
                            queue: queued,
                            latest_reviews: reviews,
                            would_notify_rejection: None,
                        },
                    )]),
                    ..Default::default()
                })
            },
            |_| unreachable!(),
        )
        .unwrap();
        current.observed_at += 1;
    }
    let saved = fs::read_to_string(&path).unwrap();
    fs::remove_file(path).unwrap();
    let samples: Vec<Value> = saved
        .lines()
        .map(|row| serde_json::from_str(row).unwrap())
        .collect();
    assert!(samples[0].get("matched_queue_admissions").is_none());
    let rows = &samples[1..];
    assert!(rows[0].get("matched_queue_admissions").is_none());
    assert_eq!(
        rows[1]["matched_queue_admissions"]["42"],
        json!({
            "removal_event":"removal-1","pr_head":HEAD,"merge_group_head":GROUP,
            "enqueued_at":prior.observed_at-5,"admission_observed_at":prior.observed_at,
            "rejection_baseline":{"main":first_main,"observed_at":prior.observed_at+3}
        })
    );
    assert_eq!(rows[2]["landing_main"], later_main);
    assert_eq!(
        rows[2]["matched_queue_admissions"],
        rows[1]["matched_queue_admissions"]
    );
    let notice = json!({
        "pr_head":HEAD,"merge_group_head":GROUP,"removal_event":"removal-1",
        "target":{"pull_request_id":"pr-42","author_id":"user-1","login":"sample-author"},
        "failed_tests":["example::failed", "example::failed_11", "example::failed_15"]
    });
    for row in &rows[1..=2] {
        assert_eq!(row["landing"]["42"]["next"], "wait_notification");
        assert!(row["landing"]["42"].get("would_notify_rejection").is_none());
    }
    assert_eq!(rows[3]["landing"]["42"]["next"], "wait_notification");
    assert_eq!(rows[3]["landing"]["42"]["would_notify_rejection"], notice);
    assert_eq!(rows[4]["landing"]["42"]["next"], "hold_findings");
    assert_eq!(rows[4]["landing"]["42"]["would_notify_rejection"], notice);
    assert!(rows[5].get("matched_queue_admissions").is_none());
    assert_eq!(rows[5]["queue_rejections"][0]["merge_group_head"], other);
    assert_eq!(
        rows[6]["matched_queue_admissions"],
        rows[1]["matched_queue_admissions"]
    );
    assert_eq!(rows[6]["landing"]["42"]["next"], "already_queued");
    assert!(rows[6]["landing"]["42"].get("would_notify_rejection").is_none());
    assert!(rows[7].get("matched_queue_admissions").is_none());
    assert_eq!(rows[7]["queue_rejections"][0]["merge_group_head"], other);
    assert_eq!(
        rows[7]["queue_rejections"][0]["removed_at"],
        "2026-01-01T00:00:01Z"
    );
    assert!(rows[8].get("matched_queue_admissions").is_none());
    assert_eq!(rows[8]["queue_rejections"][0]["merge_group_head"], GROUP);
    for row in [&rows[5], &rows[7], &rows[8]] {
        assert_eq!(row["landing"]["42"]["next"], "wait_queue");
        assert!(row["landing"]["42"].get("would_notify_rejection").is_none());
    }
}

#[test]
fn failed_required_workflows_belong_to_the_removed_commit() {
    let mut data = fixture("FAILURE");
    let checks = data
        .pointer_mut("/p42/timelineItems/nodes/0/beforeCommit/statusCheckRollup/contexts/nodes")
        .unwrap()
        .as_array_mut()
        .unwrap();
    let suite = json!({
        "databaseId": 101,
        "commit": {"oid": GROUP},
        "workflowRun": {"databaseId": 201, "event": "merge_group", "runAttempt": 2}
    });
    // Install the same suite on optional, successful, incomplete and late runs too.
    for check in checks.iter_mut().take(6) {
        check["checkSuite"] = suite.clone();
    }
    checks[0]["checkSuite"]["commit"]["oid"] = json!(HEAD);
    let read = |data: &Value| {
        let evidence = queue::read(&board(), |fields| {
            assert!(fields
                .contains("checkSuite{databaseId commit{oid} workflowRun{databaseId event runAttempt}}"));
            Ok((data.clone(), 4096))
        })
        .unwrap();
        serde_json::to_value(&evidence[0]).unwrap()
    };
    let evidence = read(&data);
    assert_eq!(evidence["failed_required_check_runs"], json!([11, 15]));
    assert_eq!(
        evidence["failed_required_workflows"],
        json!({"11": {"check_suite_id": 101, "workflow_run_id": 201, "observed_run_attempt": 2}})
    );
    let workflow_path =
        "/p42/timelineItems/nodes/0/beforeCommit/statusCheckRollup/contexts/nodes/1/checkSuite/workflowRun";
    for (field, value) in [("event", json!("pull_request")), ("runAttempt", json!(0))] {
        let mut malformed = data.clone();
        malformed.pointer_mut(workflow_path).unwrap()[field] = value;
        let evidence = read(&malformed);
        assert_eq!(evidence["failed_required_check_runs"], json!([11, 15]));
        assert!(evidence.get("failed_required_workflows").is_none());
    }
}

#[test]
fn landing_publication_reconciles_an_acknowledged_retry_and_alarms_on_its_exact_group() {
    use oxa_work::board::{
        landing::{LatestReviews, Next},
        queue::report::{VerifiedArchive, VerifiedReports},
    };
    use oxa_work::RunPolicy;
    use shadow::{publication, LandingDecision, LandingSample, QueueObservation};
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt};

    const ADMITTED_GROUP: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const ORIGINAL_MAIN: &str = "dddddddddddddddddddddddddddddddddddddddd";
    const ADVANCED_MAIN: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    const START: u64 = 1_767_225_600;
    let temp = private_dir::PrivateDir::new("landing-receipt-roundtrip");
    let repo = temp.path();
    let journal = repo.join("owner.jsonl");
    let git = repo.join("git");
    let gh = repo.join("gh");
    fs::write(
        &git,
        "#!/bin/sh\nprintf '%s\\n' https://example.invalid/sample/repo\n",
    )
    .unwrap();
    fs::write(&gh, r#"#!/bin/sh
case "$*" in
  'repo view https://example.invalid/sample/repo --json id,nameWithOwner,url')
    printf '%s\n' '{"id":"repo-1","nameWithOwner":"sample/repo","url":"https://example.invalid/sample/repo"}' ;;
  'api graphql --hostname example.invalid -f query=query{viewer{id}}')
    printf '%s\n' '{"data":{"viewer":{"id":"publisher-1"}}}' ;;
  'api graphql --hostname example.invalid -f query=mutation('*' pr=pr-42 -f body=@sample-author '*example::failed*)
    printf '%s\n' notice >> mutations
    printf '%s\n' '{"data":{"addComment":{"commentEdge":{"node":{"id":"comment-1",
        "repository":{"id":"repo-1"},"pullRequest":{"id":"pr-42","author":{"id":"user-1","login":"sample-author"},
        "headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
        "author":{"__typename":"User","id":"publisher-1"},"createdAt":"2026-01-01T00:00:05Z"}}}}}' ;;
  'api graphql --hostname example.invalid -f query=mutation('*' pr=pr-42 -f head=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa')
    printf '%s\n' enqueue >> mutations
    /bin/cat admission.json ;;
  *) exit 1 ;;
esac
"#).unwrap();
    for binary in [&git, &gh] {
        fs::set_permissions(binary, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let admission = json!({
        "id":"entry-2","enqueuedAt":"2026-01-01T00:01:00Z",
        "mergeQueue":{"id":"queue-1","repository":{"id":"repo-1"}},
        "pullRequest":{"id":"pr-42","number":42,"headRefOid":HEAD},
        "enqueuer":{"__typename":"User","id":"publisher-1"},
        "headCommit":null
    });
    fs::write(
        repo.join("admission.json"),
        json!({"data":{"enqueuePullRequest":{"mergeQueueEntry":admission}}}).to_string(),
    )
    .unwrap();

    let sample = |observed_at, main: &str, reviews, queue: Option<QueueObservation>, rejections| {
        let mut board = board();
        board.observed_at = observed_at;
        LandingSample {
            board,
            queue_rejections: rejections,
            landing_main: Some(main.into()),
            landing: BTreeMap::from([(
                42,
                LandingDecision {
                    next: Next::WaitQueue,
                    queue_alarm: false,
                    repeat_rejection_alarm: false,
                    queue,
                    latest_reviews: reviews,
                    would_notify_rejection: None,
                },
            )]),
            ..Default::default()
        }
    };
    let rejected = |group: &str, removal: &str, removed_at: &str| {
        let mut evidence = fixture("FAILURE");
        evidence["id"] = "repo-1".into();
        let pr = &mut evidence["p42"];
        pr["id"] = "pr-42".into();
        pr["author"] = json!({"__typename":"User","id":"user-1","login":"sample-author"});
        pr["mergeQueueEntry"] = Value::Null;
        pr["admissions"] = json!({
            "pageInfo":{"hasPreviousPage":false,"hasNextPage":false},
            "nodes":[{"__typename":"AddedToMergeQueueEvent","id":"enqueued-1"}]
        });
        let event = &mut pr["timelineItems"]["nodes"][0];
        event["id"] = removal.into();
        event["createdAt"] = removed_at.into();
        let commit = &mut event["beforeCommit"];
        commit["oid"] = group.into();
        let rollup = &mut commit["statusCheckRollup"];
        rollup["commit"]["oid"] = group.into();
        let mut checks = vec![
            check(11, true, "COMPLETED", "FAILURE", removed_at),
            check(15, true, "COMPLETED", "FAILURE", removed_at),
        ];
        for check in &mut checks {
            check["checkSuite"] = json!({
                "databaseId":51, "commit":{"oid":group},
                "workflowRun":{"databaseId":61,"event":"merge_group","runAttempt":1}
            });
        }
        rollup["contexts"] = json!({"totalCount":checks.len(),"nodes":checks});
        let mut result = queue::read(&board(), |fields| {
            assert!(fields.contains("p42:pullRequest(number:42)"));
            Ok((evidence.clone(), 4096))
        })
        .unwrap();
        result[0].record_verified_reports(VerifiedReports {
            failed_check_runs: vec![11, 15],
            workflow_run: 61,
            run_attempt: 1,
            archives: vec![VerifiedArchive {
                id: 71,
                sha256: "a".repeat(64),
            }],
            failed_tests: vec!["example::failed".into()],
        });
        result
    };
    let observe = |sample| {
        let mut log = shadow::open_log(&journal).unwrap();
        let mut sample = Some(sample);
        shadow::run_with_landing(&mut log, true, || Ok(sample.take().unwrap()), |_| unreachable!()).unwrap();
    };

    observe(sample(
        START,
        ORIGINAL_MAIN,
        LatestReviews::Clear,
        Some(QueueObservation {
            enqueued_at: START - 1900,
            group_head: Some(GROUP.into()),
        }),
        vec![],
    ));
    let run = RunPolicy {
        git_binary: git,
        host_path: Default::default(),
        terminate_binary: None,
        offline: false,
        runner_owns_github: false,
        storage: repo.to_owned(),
    };
    let policy = publication::Policy {
        enabled: true,
        publisher_id: "publisher-1".into(),
    };
    // The Reader saw the removal before dispatch, but GitHub reports both
    // the removal and the resulting comment in the same UTC second.
    let first = rejected(GROUP, "removal-1", "2026-01-01T00:00:05Z");
    for (at, main, review, expected_mutations) in [
        (START + 5, ORIGINAL_MAIN, LatestReviews::Clear, "notice\n"),
        (START + 59, ADVANCED_MAIN, LatestReviews::Unresolved, "notice\n"),
        (
            START + 60,
            ADVANCED_MAIN,
            LatestReviews::Clear,
            "notice\nenqueue\n",
        ),
    ] {
        publication::run_once(
            &policy,
            &journal,
            repo,
            &run,
            &gh,
            || Ok(sample(at, main, review, None, first.clone())),
            |_| panic!("a rejection snapshot cannot reconcile an admission"),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(repo.join("mutations")).unwrap(),
            expected_mutations
        );
    }

    for (step, entry_id) in [(1, "unrelated-entry"), (2, "entry-2")] {
        let mut returned = admission.clone();
        returned["id"] = entry_id.into();
        returned["headCommit"] = json!({"oid":ADMITTED_GROUP});
        let output = publication::run_once(
            &policy,
            &journal,
            repo,
            &run,
            &gh,
            || {
                Ok(sample(
                    START + 60 + step,
                    ADVANCED_MAIN,
                    LatestReviews::Clear,
                    Some(QueueObservation {
                        enqueued_at: START + 60,
                        group_head: Some(ADMITTED_GROUP.into()),
                    }),
                    vec![],
                ))
            },
            |ack| {
                queue::read_acknowledged_admission(ack, |fields| {
                    assert!(fields.contains("pullRequest(number:42)"));
                    Ok((
                        json!({"id":"repo-1","mergeQueue":{"id":"queue-1"},
                            "pullRequest":{"mergeQueueEntry":returned}}),
                        4096,
                    ))
                })
            },
        );
        assert_eq!(output.is_ok(), step == 2, "{output:?}");
    }
    observe(sample(
        START + 121,
        ADVANCED_MAIN,
        LatestReviews::Clear,
        None,
        rejected(ADMITTED_GROUP, "removal-2", "2026-01-01T00:02:00Z"),
    ));
    let rows: Vec<Value> = fs::read_to_string(journal)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let steps: Vec<_> = rows
        .iter()
        .filter_map(|row| row["landing_publication"]["step"].as_str())
        .collect();
    assert_eq!(
        steps,
        [
            "notice_reserved",
            "delivered",
            "retry_reserved",
            "acknowledged",
            "admitted"
        ]
    );
    assert_eq!(
        rows.last().unwrap()["landing"]["42"]["repeat_rejection_alarm"],
        true
    );
    assert_eq!(
        fs::read_to_string(repo.join("mutations")).unwrap(),
        "notice\nenqueue\n"
    );
}
