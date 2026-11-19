use oxa_work::board::{self, landing::*, *};
use std::collections::BTreeMap;

const EVENT: &str = "removal-1";
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const FIRST_GROUP: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const RETRY_GROUP: &str = "cccccccccccccccccccccccccccccccccccccccc";
const OTHER_GROUP: &str = "dddddddddddddddddddddddddddddddddddddddd";
const MAIN_BEFORE: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const MAIN_AFTER: &str = "ffffffffffffffffffffffffffffffffffffffff";
const MAIN_NEWER: &str = "1111111111111111111111111111111111111111";
const LAST_ADMISSION_AT: u64 = 10_000;

struct Snapshot(PullRequest);
impl GitHub for Snapshot {
    fn open_pull_requests(&mut self) -> oxa_work::Result<Vec<PullRequest>> {
        Ok(vec![self.0.clone()])
    }
}
fn board() -> Board {
    let pr = PullRequest {
        number: 1,
        head: HEAD.into(),
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
        head: HEAD,
        main: MAIN_BEFORE,
        merge_group_head: FIRST_GROUP,
        cause: RejectionCause::FailedTests(&["synthetic_regression"]),
        removal_event: EVENT,
        notified_event: None,
        retry: Retry::Unused {
            last_admission_at: LAST_ADMISSION_AT,
        },
    }
}

#[test]
fn default_off_and_latest_findings_override_approved_ready_head() {
    let board = board();
    let mut observations = BTreeMap::from([(
        1,
        Observation {
            head: HEAD,
            reviews: LatestReviews::Unresolved,
            queue: Queue::Absent,
        },
    )]);
    assert!(board
        .plan_landing(board::landing::Policy::default(), &observations, MAIN_BEFORE)
        .is_empty());
    let next =
        |observations: &BTreeMap<_, _>| board.plan_landing(policy(), observations, MAIN_BEFORE)[&1].next;
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
fn incomplete_evidence_is_excluded_from_landing_plans() {
    let mut board = board();
    board.entries[0].evidence_incomplete = Some("review threads exceed the page".into());
    let observations = BTreeMap::from([(
        1,
        Observation {
            head: HEAD,
            reviews: LatestReviews::Unresolved,
            queue: Queue::Queued { since: 1 },
        },
    )]);
    assert!(board
        .plan_landing(policy(), &observations, MAIN_BEFORE)
        .is_empty());
}

#[test]
fn rejected_head_notifies_with_failing_tests_then_retries_once_after_main_moves() {
    let entry = ready();
    let mut rejected = rejection();
    let plan = |receipt, main| {
        policy().plan(
            &entry,
            LatestReviews::Clear,
            Queue::Rejected(receipt),
            main,
            LAST_ADMISSION_AT + 1800,
        )
    };
    let notice = plan(rejected, MAIN_AFTER);
    assert_eq!(
        notice.notify_rejection,
        Some(Notice {
            head: HEAD,
            merge_group_head: FIRST_GROUP,
            removal_event: EVENT,
            cause: RejectionCause::FailedTests(&["synthetic_regression"]),
        })
    );
    assert_eq!(notice.next, Next::WaitNotification);
    let missing_tests = plan(
        Rejection {
            cause: RejectionCause::FailedTests(&[]),
            ..rejected
        },
        MAIN_AFTER,
    );
    assert_eq!(missing_tests.notify_rejection, None);
    assert_eq!(missing_tests.next, Next::WaitNotification);
    let conflict = plan(
        Rejection {
            cause: RejectionCause::MergeConflict,
            ..rejected
        },
        MAIN_AFTER,
    );
    assert_eq!(
        conflict.notify_rejection,
        Some(Notice {
            head: HEAD,
            merge_group_head: FIRST_GROUP,
            removal_event: EVENT,
            cause: RejectionCause::MergeConflict,
        })
    );
    assert_eq!(conflict.next, Next::WaitNotification);
    rejected.notified_event = Some(EVENT);
    assert_eq!(plan(rejected, MAIN_BEFORE).next, Next::WaitMain);
    assert_eq!(plan(rejected, MAIN_AFTER).next, Next::Retry);
    rejected.retry = Retry::Unknown;
    assert_eq!(plan(rejected, MAIN_AFTER).next, Next::WaitReconciliation);
    rejected.retry = Retry::Consumed {
        first_rejected_group: FIRST_GROUP,
        admitted_group: None,
    };
    let exhausted = plan(rejected, MAIN_NEWER);
    assert_eq!(exhausted.next, Next::RetryExhausted);
    assert_eq!(exhausted.notify_rejection, None);
    assert!(!exhausted.repeat_rejection_alarm);
    rejected.head = "old-head";
    assert_eq!(plan(rejected, MAIN_NEWER).next, Next::WaitReconciliation);
}

#[test]
fn an_earlier_delivery_receipt_does_not_unlock_a_later_removal() {
    let entry = ready();
    let plan = |rejected| {
        policy().plan(
            &entry,
            LatestReviews::Clear,
            Queue::Rejected(rejected),
            MAIN_AFTER,
            LAST_ADMISSION_AT + 1800,
        )
    };
    let mut rejected = Rejection {
        removal_event: "removal-2",
        notified_event: Some(EVENT),
        ..rejection()
    };
    let pending = plan(rejected);
    assert_eq!(pending.next, Next::WaitNotification);
    assert_eq!(pending.notify_rejection.unwrap().removal_event, "removal-2");

    rejected.notified_event = Some("removal-2");
    let delivered = plan(rejected);
    assert_eq!(delivered.next, Next::Retry);
    assert_eq!(delivered.notify_rejection, None);
}

#[test]
fn invalid_removal_event_cannot_notify_retry_or_alarm() {
    let entry = ready();
    for removal_event in ["", "not a node id"] {
        let plan = policy().plan(
            &entry,
            LatestReviews::Clear,
            Queue::Rejected(Rejection {
                removal_event,
                notified_event: Some(removal_event),
                retry: Retry::Consumed {
                    first_rejected_group: OTHER_GROUP,
                    admitted_group: Some(FIRST_GROUP),
                },
                ..rejection()
            }),
            MAIN_AFTER,
            LAST_ADMISSION_AT + 1800,
        );
        assert_eq!(plan.next, Next::WaitReconciliation);
        assert_eq!(plan.notify_rejection, None);
        assert!(!plan.repeat_rejection_alarm);
    }
}

#[test]
fn malformed_main_observations_cannot_enqueue_or_retry() {
    let entry = ready();
    let now = LAST_ADMISSION_AT + 1800;
    let plan = |queue, main| policy().plan(&entry, LatestReviews::Clear, queue, main, now).next;
    let known = Rejection {
        notified_event: Some(EVENT),
        ..rejection()
    };
    for missing in ["", "main", "abcdef0"] {
        assert_eq!(plan(Queue::Absent, missing), Next::WaitReconciliation);
        assert_eq!(plan(Queue::Rejected(known), missing), Next::WaitReconciliation);
        assert_eq!(
            plan(
                Queue::Rejected(Rejection {
                    main: missing,
                    ..known
                }),
                MAIN_AFTER
            ),
            Next::WaitReconciliation
        );
    }
}

#[test]
fn retry_spacing_is_measured_from_the_admission_of_the_rejected_head() {
    let entry = ready();
    let mut rejected = rejection();
    rejected.notified_event = Some(EVENT);
    let plan = |receipt, main, now| {
        policy()
            .plan(&entry, LatestReviews::Clear, Queue::Rejected(receipt), main, now)
            .next
    };
    for (elapsed, next) in [
        (0, Next::WaitRetrySpacing),
        (1799, Next::WaitRetrySpacing),
        (1800, Next::Retry),
    ] {
        assert_eq!(plan(rejected, MAIN_AFTER, LAST_ADMISSION_AT + elapsed), next);
    }
    assert_eq!(
        plan(rejected, MAIN_BEFORE, LAST_ADMISSION_AT + 1801),
        Next::WaitMain
    );
    assert_eq!(
        plan(rejected, MAIN_AFTER, LAST_ADMISSION_AT - 1),
        Next::WaitReconciliation
    );
    rejected.retry = Retry::Unused { last_admission_at: 0 };
    assert_eq!(
        plan(rejected, MAIN_AFTER, LAST_ADMISSION_AT),
        Next::WaitReconciliation
    );
}

#[test]
fn rejection_with_unbound_head_or_group_cannot_notify_or_retry() {
    let entry = ready();
    for (head, group) in [
        ("old-head", FIRST_GROUP),
        (entry.head.as_str(), ""),
        (entry.head.as_str(), "not-an-object-id"),
        (entry.head.as_str(), entry.head.as_str()),
    ] {
        for notified_event in [None, Some(EVENT)] {
            let plan = policy().plan(
                &entry,
                LatestReviews::Clear,
                Queue::Rejected(Rejection {
                    head,
                    merge_group_head: group,
                    notified_event,
                    ..rejection()
                }),
                MAIN_AFTER,
                0,
            );
            assert_eq!(plan.notify_rejection, None, "head={head:?} group={group:?}");
            assert_eq!(
                plan.next,
                Next::WaitReconciliation,
                "head={head:?} group={group:?}"
            );
            assert!(!plan.repeat_rejection_alarm);
        }
    }
}

#[test]
fn repeat_rejection_alarm_requires_the_retried_group_for_the_same_head() {
    let entry = ready();
    let mut rejected = rejection();
    rejected.retry = Retry::Consumed {
        first_rejected_group: FIRST_GROUP,
        admitted_group: Some(RETRY_GROUP),
    };
    let plan = |receipt| {
        policy().plan(
            &entry,
            LatestReviews::Unresolved,
            Queue::Rejected(receipt),
            MAIN_AFTER,
            0,
        )
    };
    assert!(!plan(rejected).repeat_rejection_alarm);
    rejected.merge_group_head = OTHER_GROUP;
    assert!(!plan(rejected).repeat_rejection_alarm);
    rejected.merge_group_head = RETRY_GROUP;
    assert!(plan(rejected).repeat_rejection_alarm);
    assert_eq!(plan(rejected).next, Next::HoldFindings);
    assert!(
        !board::landing::Policy::default()
            .plan(
                &entry,
                LatestReviews::Clear,
                Queue::Rejected(rejected),
                MAIN_AFTER,
                0
            )
            .repeat_rejection_alarm
    );
    assert!(
        !plan(Rejection {
            head: "different-head",
            ..rejected
        })
        .repeat_rejection_alarm
    );
    let missing_report = plan(Rejection {
        cause: RejectionCause::Unknown,
        ..rejected
    });
    assert!(missing_report.repeat_rejection_alarm);
    assert!(missing_report.notify_rejection.is_none());
    rejected.retry = Retry::Consumed {
        first_rejected_group: RETRY_GROUP,
        admitted_group: Some(RETRY_GROUP),
    };
    assert!(!plan(rejected).repeat_rejection_alarm);
}

#[test]
fn queue_alarm_is_strictly_older_than_45_minutes_without_duplicate_enqueue() {
    let entry = ready();
    for (now, alarm) in [(99, false), (2800, false), (2801, true)] {
        let plan = policy().plan(
            &entry,
            LatestReviews::Clear,
            Queue::Queued { since: 100 },
            MAIN_BEFORE,
            now,
        );
        assert_eq!(plan.next, Next::AlreadyQueued);
        assert_eq!(plan.queue_alarm, alarm);
    }
    let plan = policy().plan(
        &entry,
        LatestReviews::Unresolved,
        Queue::Queued { since: 100 },
        MAIN_BEFORE,
        2801,
    );
    assert_eq!(plan.next, Next::HoldFindings);
    assert!(plan.queue_alarm);
}

#[test]
fn main_movement_never_overrides_existing_board_gates_or_asks_for_a_main_merge() {
    let mut entry = ready();
    let mut rejected = rejection();
    rejected.notified_event = Some(EVENT);
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
                MAIN_AFTER,
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
            .plan(&entry, LatestReviews::Clear, Queue::Absent, MAIN_AFTER, 0)
            .next,
        Next::WaitBoard
    );
}
