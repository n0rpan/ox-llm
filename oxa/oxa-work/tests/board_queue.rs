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
