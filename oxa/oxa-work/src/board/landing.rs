//! Landing decisions derived from a board snapshot and trusted queue observations.
use super::{Action, Board, Entry};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default)]
pub struct Policy {
    pub enabled: bool,
}

/// The latest review outcome for each required reviewer, including older heads.
/// Clear only describes findings. The board supplies the independent approval gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatestReviews {
    Unknown,
    Clear,
    Unresolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectionCause<'a> {
    Unknown,
    MergeConflict,
    FailedTests(&'a [&'a str]),
}

impl RejectionCause<'_> {
    fn known(self) -> bool {
        !matches!(self, Self::Unknown | Self::FailedTests([]))
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Retry<'a> {
    Unused,
    /// Durable retry receipt for this PR head. Group identities come from the
    /// queue observations before and after retry admission.
    Consumed {
        first_rejected_group: &'a str,
        admitted_group: Option<&'a str>,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Rejection<'a> {
    pub head: &'a str,
    pub main: &'a str,
    pub merge_group_head: &'a str,
    /// Failed test names must come from the rejected merge-group run.
    pub cause: RejectionCause<'a>,
    pub notified: bool,
    pub retry: Retry<'a>,
}

#[derive(Clone, Copy, Debug)]
pub enum Queue<'a> {
    Unknown,
    Absent,
    Queued { since: u64 },
    Rejected(Rejection<'a>),
}

#[derive(Clone, Copy, Debug)]
pub struct Observation<'a> {
    /// PR head at which the caller acquired these observations. Review findings
    /// themselves may have been recorded on earlier heads.
    pub head: &'a str,
    pub reviews: LatestReviews,
    pub queue: Queue<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Next {
    Disabled,
    HoldFindings,
    WaitReview,
    WaitBoard,
    WaitQueue,
    AlreadyQueued,
    WaitNotification,
    WaitMain,
    WaitReconciliation,
    RetryExhausted,
    Enqueue,
    Retry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Notice<'a> {
    pub head: &'a str,
    pub cause: RejectionCause<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan<'a> {
    pub next: Next,
    pub notify_rejection: Option<Notice<'a>>,
    pub queue_alarm: bool,
    pub repeat_rejection_alarm: bool,
}

impl Board {
    /// Uses the board's observation time for the queue-age threshold. Missing
    /// observations hold landing. Issue intake items have no landing decision.
    pub fn plan_landing<'a>(
        &self,
        policy: Policy,
        observations: &BTreeMap<u64, Observation<'a>>,
        main: &str,
    ) -> BTreeMap<u64, Plan<'a>> {
        if !policy.enabled {
            return BTreeMap::new();
        }
        self.entries
            .iter()
            .filter(|entry| !matches!(entry.action, Action::BuildIssue | Action::WaitIssue))
            .map(|entry| {
                let (reviews, queue) = observations
                    .get(&entry.number)
                    .filter(|facts| facts.head == entry.head)
                    .map_or((LatestReviews::Unknown, Queue::Unknown), |facts| {
                        (facts.reviews, facts.queue)
                    });
                (
                    entry.number,
                    policy.plan(entry, reviews, queue, main, self.observed_at),
                )
            })
            .collect()
    }
}

impl Policy {
    /// A plan is read-only. Before execution the board owner must revalidate
    /// the source facts and fence notification and retry with durable receipts.
    pub fn plan<'a>(
        &self,
        entry: &Entry,
        reviews: LatestReviews,
        queue: Queue<'a>,
        main: &str,
        now: u64,
    ) -> Plan<'a> {
        if !self.enabled {
            return Plan {
                next: Next::Disabled,
                notify_rejection: None,
                queue_alarm: false,
                repeat_rejection_alarm: false,
            };
        }
        let notify_rejection = match queue {
            Queue::Rejected(r) if !r.notified && !r.head.is_empty() && r.cause.known() => Some(Notice {
                head: r.head,
                cause: r.cause,
            }),
            _ => None,
        };
        let repeat_rejection_alarm = match queue {
            Queue::Rejected(Rejection {
                head,
                merge_group_head,
                retry:
                    Retry::Consumed {
                        first_rejected_group,
                        admitted_group: Some(retry_group),
                    },
                ..
            }) => {
                !head.is_empty()
                    && head == entry.head
                    && !first_rejected_group.is_empty()
                    && !retry_group.is_empty()
                    && retry_group != first_rejected_group
                    && retry_group == merge_group_head
            }
            _ => false,
        };
        let next = match (reviews, queue) {
            (LatestReviews::Unresolved, _) => Next::HoldFindings,
            _ if entry.action == Action::FixFindings => Next::HoldFindings,
            (LatestReviews::Unknown, _) => Next::WaitReview,
            (_, Queue::Queued { .. }) => Next::AlreadyQueued,
            _ if entry.action != Action::Ready || !entry.eligible => Next::WaitBoard,
            (_, Queue::Unknown) => Next::WaitQueue,
            (_, Queue::Absent) if !main.is_empty() => Next::Enqueue,
            (_, Queue::Absent) => Next::WaitReconciliation,
            (_, Queue::Rejected(r))
                if r.head != entry.head || r.head.is_empty() || r.main.is_empty() || main.is_empty() =>
            {
                Next::WaitReconciliation
            }
            (_, Queue::Rejected(r)) if !r.notified || !r.cause.known() => Next::WaitNotification,
            (_, Queue::Rejected(r)) if matches!(r.retry, Retry::Consumed { .. }) => Next::RetryExhausted,
            (_, Queue::Rejected(r)) if r.main == main => Next::WaitMain,
            (_, Queue::Rejected(_)) => Next::Retry,
        };
        Plan {
            next,
            notify_rejection,
            queue_alarm: matches!(queue, Queue::Queued { since } if now.saturating_sub(since) > 45 * 60),
            repeat_rejection_alarm,
        }
    }
}
