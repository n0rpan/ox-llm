//! Landing plans only: acquisition, notification, retry receipts and GitHub writes
//! remain with the existing board owner. No branch update is planned speculatively.
use super::{Action, Entry};

#[derive(Clone, Copy, Debug, Default)]
pub struct Policy {
    pub enabled: bool,
}

/// The latest required review summaries, including reviews on older heads.
/// Clear means no unresolved findings, not that approval requirements are met;
/// the existing board's Ready action still supplies that gate. Adapters must
/// consider every required review kind, not let a code review clear security.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatestReviews {
    Unknown,
    Clear,
    Unresolved,
}

#[derive(Clone, Copy, Debug)]
pub struct Rejection<'a> {
    pub head: &'a str,
    pub main: &'a str,
    /// Exact failing test names from the queue receipt, for the author's notice.
    pub failing_tests: &'a [&'a str],
    pub notified: bool,
    /// The caller retains this receipt across subsequent rejection observations.
    pub retry_used: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum Queue<'a> {
    Absent,
    Queued { since: u64 },
    Rejected(Rejection<'a>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    Disabled,
    HoldFindings,
    WaitReview,
    WaitBoard,
    AlreadyQueued,
    WaitNotification,
    WaitMain,
    WaitReconciliation,
    RetryExhausted,
    Enqueue,
    Retry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub next: Next,
    pub notify_rejection: bool,
    pub queue_alarm: bool,
}

impl Policy {
    /// `main` and `queue` must come from the same complete trusted snapshot as
    /// `entry`. A plan is not a reservation: the owner must atomically revalidate
    /// the head/main and record notification/retry before executing any write.
    /// A rejection for another head requires explicit owner reconciliation.
    pub fn plan(
        &self,
        entry: &Entry,
        reviews: LatestReviews,
        queue: Queue<'_>,
        main: &str,
        now: u64,
    ) -> Plan {
        let mut plan = Plan {
            next: Next::Disabled,
            notify_rejection: false,
            queue_alarm: false,
        };
        if !self.enabled {
            return plan;
        }
        plan.notify_rejection = matches!(queue, Queue::Rejected(r) if !r.notified);
        plan.queue_alarm = matches!(queue, Queue::Queued { since } if now.saturating_sub(since) > 45 * 60);
        plan.next = if reviews == LatestReviews::Unresolved || entry.action == Action::FixFindings {
            Next::HoldFindings
        } else if reviews == LatestReviews::Unknown {
            Next::WaitReview
        } else if let Queue::Queued { .. } = queue {
            Next::AlreadyQueued
        } else if entry.action != Action::Ready || !entry.eligible {
            Next::WaitBoard
        } else {
            match queue {
                Queue::Absent => Next::Enqueue,
                Queue::Queued { .. } => unreachable!(),
                Queue::Rejected(r)
                    if r.head != entry.head || r.head.is_empty() || r.main.is_empty() || main.is_empty() =>
                {
                    Next::WaitReconciliation
                }
                Queue::Rejected(r) if !r.notified => Next::WaitNotification,
                Queue::Rejected(r) if r.retry_used => Next::RetryExhausted,
                Queue::Rejected(r) if r.main == main => Next::WaitMain,
                Queue::Rejected(_) => Next::Retry,
            }
        };
        plan
    }
}
