//! Landing decisions derived from a board snapshot and trusted queue observations.
use super::{
    queue::{is_node_id, is_object_id},
    Action, Board, Entry,
};
use std::collections::BTreeMap;

const MIN_RETRY_SPACING_SECS: u64 = 30 * 60;

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
    /// The caller has no authoritative receipt history for this PR head.
    Unknown,
    /// The owner has reconciled the retry ledger and verified the last
    /// admission of this PR head. Sample time alone is insufficient.
    Unused { last_admission_at: u64 },
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
    pub removal_event: &'a str,
    /// Failed test names must come from the rejected merge-group run.
    pub cause: RejectionCause<'a>,
    /// The removal event named by a verified author-delivery receipt.
    pub notified_event: Option<&'a str>,
    pub retry: Retry<'a>,
}

impl Rejection<'_> {
    fn matches_entry(&self, entry: &Entry) -> bool {
        self.head == entry.head
            && is_object_id(self.head)
            && self.merge_group_head != self.head
            && is_object_id(self.merge_group_head)
            && is_node_id(self.removal_event)
    }

    fn notified(&self) -> bool {
        self.notified_event == Some(self.removal_event)
    }
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
    WaitRetrySpacing,
    RetryExhausted,
    Enqueue,
    Retry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Notice<'a> {
    pub head: &'a str,
    pub merge_group_head: &'a str,
    pub removal_event: &'a str,
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
            Queue::Rejected(r) if r.matches_entry(entry) && !r.notified() && r.cause.known() => {
                Some(Notice {
                    head: r.head,
                    merge_group_head: r.merge_group_head,
                    removal_event: r.removal_event,
                    cause: r.cause,
                })
            }
            _ => None,
        };
        let repeat_rejection_alarm = match queue {
            Queue::Rejected(r) if r.matches_entry(entry) => matches!(
                r.retry,
                Retry::Consumed {
                    first_rejected_group,
                    admitted_group: Some(retry_group),
                } if is_object_id(first_rejected_group)
                    && first_rejected_group != r.head
                    && retry_group != first_rejected_group
                    && retry_group == r.merge_group_head
            ),
            _ => false,
        };
        let next = match (reviews, queue) {
            (LatestReviews::Unresolved, _) => Next::HoldFindings,
            _ if entry.action == Action::FixFindings => Next::HoldFindings,
            (LatestReviews::Unknown, _) => Next::WaitReview,
            (_, Queue::Queued { .. }) => Next::AlreadyQueued,
            _ if entry.action != Action::Ready || !entry.eligible => Next::WaitBoard,
            (_, Queue::Unknown) => Next::WaitQueue,
            (_, Queue::Absent) if is_object_id(main) => Next::Enqueue,
            (_, Queue::Absent) => Next::WaitReconciliation,
            (_, Queue::Rejected(r))
                if !r.matches_entry(entry) || !is_object_id(r.main) || !is_object_id(main) =>
            {
                Next::WaitReconciliation
            }
            (_, Queue::Rejected(r)) if !r.notified() || !r.cause.known() => Next::WaitNotification,
            (_, Queue::Rejected(r)) => match r.retry {
                Retry::Unknown => Next::WaitReconciliation,
                Retry::Consumed { .. } => Next::RetryExhausted,
                Retry::Unused { last_admission_at } if last_admission_at == 0 || last_admission_at > now => {
                    Next::WaitReconciliation
                }
                Retry::Unused { .. } if r.main == main => Next::WaitMain,
                Retry::Unused { last_admission_at } if now - last_admission_at < MIN_RETRY_SPACING_SECS => {
                    Next::WaitRetrySpacing
                }
                Retry::Unused { .. } => Next::Retry,
            },
        };
        Plan {
            next,
            notify_rejection,
            queue_alarm: matches!(queue, Queue::Queued { since } if now.saturating_sub(since) > 45 * 60),
            repeat_rejection_alarm,
        }
    }
}
