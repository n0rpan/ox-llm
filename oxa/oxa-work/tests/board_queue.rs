use oxa_work::{
    board::{queue, Action, Board, Entry},
    shadow,
};
use serde_json::{json, Value};
use std::{fs, time::SystemTime};

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
fn changed_pr_head_refuses_the_snapshot() {
    let mut data = fixture("FAILURE");
    data["p42"]["headRefOid"] = json!(GROUP);
    assert!(queue::read(&board(), |_| Ok((data.clone(), 4096))).is_err());
}

#[test]
fn the_owner_journal_matches_only_the_previously_queued_head_and_group() {
    use oxa_work::board::landing::Next;
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
    for (head, group, main, newly_queued) in [
        (HEAD, GROUP, first_main, false),
        (HEAD, other, later_main, false),
        (HEAD, GROUP, later_main, false),
        (HEAD, GROUP, later_main, true),
        (HEAD, other, later_main, false),
        (other, GROUP, later_main, false),
    ] {
        let mut log = shadow::open_log(&path).unwrap();
        current.entries[0].head = head.into();
        let mut evidence = fixture("FAILURE");
        evidence["p42"]["headRefOid"] = head.into();
        let group_event = &mut evidence["p42"]["timelineItems"]["nodes"][0]["beforeCommit"];
        group_event["oid"] = group.into();
        group_event["statusCheckRollup"]["commit"]["oid"] = group.into();
        let rejections = queue::read(&current, |_| Ok((evidence.clone(), 4096))).unwrap();
        shadow::run_with_landing(
            &mut log,
            true,
            || {
                let queued = newly_queued.then(|| QueueObservation {
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
                            } else {
                                Next::WaitQueue
                            },
                            queue_alarm: false,
                            queue: queued,
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
    assert_eq!(rows[3]["landing_main"], later_main);
    assert_eq!(
        rows[3]["matched_queue_admissions"],
        rows[1]["matched_queue_admissions"]
    );
    assert_eq!(rows[2]["queue_rejections"][0]["merge_group_head"], other);
    assert_eq!(
        rows[4]["matched_queue_admissions"],
        rows[1]["matched_queue_admissions"]
    );
    assert_eq!(rows[4]["landing"]["42"]["next"], "already_queued");
    assert_eq!(rows[5]["queue_rejections"][0]["merge_group_head"], other);
    assert_eq!(
        rows[5]["queue_rejections"][0]["removed_at"],
        "2026-01-01T00:00:01Z"
    );
    assert_eq!(rows[6]["queue_rejections"][0]["merge_group_head"], GROUP);
    for row in [&rows[2], &rows[5], &rows[6]] {
        assert!(row.get("matched_queue_admissions").is_none());
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
