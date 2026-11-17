use oxa_work::board::{self, landing::*, *};
use std::collections::BTreeMap;

struct Snapshot(PullRequest);
impl GitHub for Snapshot {
    fn open_pull_requests(&mut self) -> oxa_work::Result<Vec<PullRequest>> {
        Ok(vec![self.0.clone()])
    }
}
fn ready() -> Entry {
    let pr = PullRequest {
        number: 1,
        head: "head-one".into(),
        release_pinned: false,
        findings: false,
        required_ci: RequiredCi::Passed,
        mergeability: Mergeability::Mergeable,
        body: String::new(),
        removes_source: false,
        draft: false,
        reviewed: true,
    };
    Board::read(
        &mut Snapshot(pr),
        &board::Policy {
            enabled: true,
            ..Default::default()
        },
        &BTreeMap::new(),
        0,
    )
    .unwrap()
    .entries
    .remove(0)
}
fn policy() -> board::landing::Policy {
    board::landing::Policy { enabled: true }
}
fn rejection() -> Rejection<'static> {
    Rejection {
        head: "head-one",
        main: "main-one",
        failing_tests: &["synthetic_regression"],
        notified: false,
        retry_used: false,
    }
}

#[test]
fn default_off_and_latest_findings_override_approved_ready_head() {
    let entry = ready();
    assert_eq!(
        board::landing::Policy::default()
            .plan(&entry, LatestReviews::Clear, Queue::Absent, "main-one", 0)
            .next,
        Next::Disabled
    );
    // Clear approval on the current diff cannot supersede unresolved latest
    // review findings observed on an older head.
    assert_eq!(
        policy()
            .plan(&entry, LatestReviews::Unresolved, Queue::Absent, "main-one", 0)
            .next,
        Next::HoldFindings
    );
    assert_eq!(
        policy()
            .plan(&entry, LatestReviews::Unknown, Queue::Absent, "main-one", 0)
            .next,
        Next::WaitReview
    );
    assert_eq!(
        policy()
            .plan(&entry, LatestReviews::Clear, Queue::Absent, "main-one", 0)
            .next,
        Next::Enqueue
    );
}

#[test]
fn rejected_head_notifies_with_failing_tests_then_retries_once_after_main_moves() {
    let entry = ready();
    let mut rejected = rejection();
    assert_eq!(rejected.failing_tests, ["synthetic_regression"]);
    let plan = policy().plan(
        &entry,
        LatestReviews::Clear,
        Queue::Rejected(rejected),
        "main-two",
        0,
    );
    assert!(plan.notify_rejection);
    assert_eq!(plan.next, Next::WaitNotification);
    rejected.notified = true;
    assert_eq!(
        policy()
            .plan(
                &entry,
                LatestReviews::Clear,
                Queue::Rejected(rejected),
                "main-one",
                0
            )
            .next,
        Next::WaitMain
    );
    assert_eq!(
        policy()
            .plan(
                &entry,
                LatestReviews::Clear,
                Queue::Rejected(rejected),
                "main-two",
                0
            )
            .next,
        Next::Retry
    );
    rejected.retry_used = true;
    let plan = policy().plan(
        &entry,
        LatestReviews::Clear,
        Queue::Rejected(rejected),
        "main-three",
        0,
    );
    assert_eq!(plan.next, Next::RetryExhausted);
    assert!(!plan.notify_rejection);
    rejected.head = "old-head";
    assert_eq!(
        policy()
            .plan(
                &entry,
                LatestReviews::Clear,
                Queue::Rejected(rejected),
                "main-three",
                0
            )
            .next,
        Next::WaitReconciliation
    );
}

#[test]
fn queue_alarm_is_strictly_older_than_45_minutes_without_duplicate_enqueue() {
    let entry = ready();
    for (now, alarm) in [(99, false), (2800, false), (2801, true)] {
        let plan = policy().plan(
            &entry,
            LatestReviews::Clear,
            Queue::Queued { since: 100 },
            "main-one",
            now,
        );
        assert_eq!(plan.next, Next::AlreadyQueued);
        assert_eq!(plan.queue_alarm, alarm);
    }
    let plan = policy().plan(
        &entry,
        LatestReviews::Unresolved,
        Queue::Queued { since: 100 },
        "main-one",
        2801,
    );
    assert_eq!(plan.next, Next::HoldFindings);
    assert!(plan.queue_alarm);
}

#[test]
fn main_movement_never_overrides_existing_board_gates_or_asks_for_a_main_merge() {
    let mut entry = ready();
    let mut rejected = rejection();
    rejected.notified = true;
    for action in [
        Action::FixFindings,
        Action::FixRequiredCi,
        Action::ResolveConflict,
        Action::AddRemoves,
        Action::PromoteDraft,
        Action::WaitReview,
        Action::WaitRequiredCi,
        Action::WaitMergeability,
    ] {
        entry.action = action;
        let next = policy()
            .plan(
                &entry,
                LatestReviews::Clear,
                Queue::Rejected(rejected),
                "main-two",
                0,
            )
            .next;
        assert_eq!(
            next,
            if action == Action::FixFindings {
                Next::HoldFindings
            } else {
                Next::WaitBoard
            }
        );
    }
    entry.action = Action::Ready;
    entry.eligible = false;
    assert_eq!(
        policy()
            .plan(&entry, LatestReviews::Clear, Queue::Absent, "main-two", 0)
            .next,
        Next::WaitBoard
    );
}
