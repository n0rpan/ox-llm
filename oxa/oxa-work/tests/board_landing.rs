use oxa_work::board::{self, landing::*, *};
use std::collections::BTreeMap;

struct Snapshot(PullRequest);
impl GitHub for Snapshot {
    fn open_pull_requests(&mut self) -> oxa_work::Result<Vec<PullRequest>> {
        Ok(vec![self.0.clone()])
    }
}
fn board() -> Board {
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
}
fn ready() -> Entry {
    board().entries.remove(0)
}
fn policy() -> board::landing::Policy {
    board::landing::Policy { enabled: true }
}
fn rejection() -> Rejection<'static> {
    Rejection {
        head: "head-one",
        main: "main-one",
        cause: RejectionCause::FailedTests(&["synthetic_regression"]),
        notified: false,
        retry_used: false,
    }
}

#[test]
fn default_off_and_latest_findings_override_approved_ready_head() {
    let board = board();
    let mut observations = BTreeMap::from([(
        1,
        Observation {
            head: "head-one",
            reviews: LatestReviews::Unresolved,
            queue: Queue::Absent,
        },
    )]);
    assert!(board
        .plan_landing(board::landing::Policy::default(), &observations, "main-one")
        .is_empty());
    let next =
        |observations: &BTreeMap<_, _>| board.plan_landing(policy(), observations, "main-one")[&1].next;
    // Approval on the current head can coexist with unresolved latest findings on an older head.
    assert_eq!(next(&observations), Next::HoldFindings);
    observations.get_mut(&1).unwrap().reviews = LatestReviews::Clear;
    assert_eq!(next(&observations), Next::Enqueue);
    observations.get_mut(&1).unwrap().queue = Queue::Unknown;
    assert_eq!(next(&observations), Next::WaitQueue);
    observations.get_mut(&1).unwrap().head = "stale-head";
    assert_eq!(next(&observations), Next::WaitReview);
    observations.clear();
    assert_eq!(next(&observations), Next::WaitReview);
}

#[test]
fn rejected_head_notifies_with_failing_tests_then_retries_once_after_main_moves() {
    let entry = ready();
    let mut rejected = rejection();
    let plan = |receipt, main| policy().plan(&entry, LatestReviews::Clear, Queue::Rejected(receipt), main, 0);
    let notice = plan(rejected, "main-two");
    assert_eq!(
        notice.notify_rejection,
        Some(Notice {
            head: "head-one",
            cause: RejectionCause::FailedTests(&["synthetic_regression"]),
        })
    );
    assert_eq!(notice.next, Next::WaitNotification);
    let missing_tests = plan(
        Rejection {
            cause: RejectionCause::FailedTests(&[]),
            ..rejected
        },
        "main-two",
    );
    assert_eq!(missing_tests.notify_rejection, None);
    assert_eq!(missing_tests.next, Next::WaitNotification);
    let conflict = plan(
        Rejection {
            cause: RejectionCause::MergeConflict,
            ..rejected
        },
        "main-two",
    );
    assert_eq!(
        conflict.notify_rejection,
        Some(Notice {
            head: "head-one",
            cause: RejectionCause::MergeConflict,
        })
    );
    assert_eq!(conflict.next, Next::WaitNotification);
    rejected.notified = true;
    assert_eq!(plan(rejected, "main-one").next, Next::WaitMain);
    assert_eq!(plan(rejected, "main-two").next, Next::Retry);
    rejected.retry_used = true;
    let exhausted = plan(rejected, "main-three");
    assert_eq!(exhausted.next, Next::RetryExhausted);
    assert_eq!(exhausted.notify_rejection, None);
    rejected.head = "old-head";
    assert_eq!(plan(rejected, "main-three").next, Next::WaitReconciliation);
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
