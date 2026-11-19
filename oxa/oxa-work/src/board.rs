//! Read-only GitHub board policy. Snapshot acquisition and durable claims belong to the caller.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

mod claims;
pub use claims::{
    Assignment, AssignmentBoard, AssignmentData, Claims, ExistingWork, ItemLease, ItemOwner, LeaseActivity,
    LeaseSignal, NativeBinding, ReleaseRecord, VerifiedMilestone,
};
pub mod assignment;
pub use claims::ActivityTiming;
pub use claims::{Dependency, DependencyWait};
pub mod check_recovery;
mod intake;
pub mod landing;
pub mod queue;
pub use intake::{ImportState, Issue, Snapshot};
mod goal_plan;
pub mod review;
pub use goal_plan::GoalStep;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    FixFindings,
    FixRequiredCi,
    ResolveConflict,
    AddRemoves,
    BuildIssue,
    PromoteDraft,
    Ready,
    WaitRequiredCi,
    WaitMergeability,
    WaitReview,
    WaitIssue,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequiredCi {
    Pending,
    Failed,
    Passed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mergeability {
    Unknown,
    Conflicting,
    Mergeable,
}

/// Normalized GitHub facts: findings are unresolved; required CI describes the current head.
#[derive(Clone, Deserialize, Serialize)]
pub struct PullRequest {
    pub number: u64,
    pub head: String,
    pub release_pinned: bool,
    pub findings: bool,
    pub required_ci: RequiredCi,
    pub mergeability: Mergeability,
    pub body: String,
    pub removes_source: bool,
    pub draft: bool,
    /// Required review policy is satisfied for the PR's own diff, not merely an old head.
    pub reviewed: bool,
}

impl std::fmt::Debug for PullRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PullRequest")
            .field("number", &self.number)
            .field("head", &self.head)
            .field("release_pinned", &self.release_pinned)
            .field("findings", &self.findings)
            .field("required_ci", &self.required_ci)
            .field("mergeability", &self.mergeability)
            .field("body", &"<redacted>")
            .field("removes_source", &self.removes_source)
            .field("draft", &self.draft)
            .field("reviewed", &self.reviewed)
            .finish()
    }
}

/// The adapter must return the complete open-PR snapshot or fail, never a partial page.
pub trait GitHub {
    fn open_pull_requests(&mut self) -> crate::Result<Vec<PullRequest>> {
        Err("adapter must implement a complete board snapshot".into())
    }
    /// One complete, caller-partitioned PR AND issue intake. Legacy PR adapters remain valid.
    fn snapshot(&mut self) -> crate::Result<Snapshot> {
        Ok(Snapshot {
            pull_requests: self.open_pull_requests()?,
            issues: vec![],
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Lease {
    pub holder: String,
    pub expires_at: u64,
    #[serde(default)]
    pub claimed_at: Option<u64>,
    /// UTC admission day and its non-refunded attempt ordinal identify this reservation.
    #[serde(default)]
    pub generation: (u64, u32),
}

pub fn valid_worker_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.split([':', '/']).all(crate::valid_task_id)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct AttemptState {
    pub utc_day: u64,
    pub attempts: u32,
    pub lease: Option<Lease>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Policy {
    pub enabled: bool,
    /// Advisory daily attempt threshold. A claim past it proceeds and warns the
    /// operator; zero, or `enabled: false`, admits nothing.
    pub max_attempts_per_day: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_attempts_per_day: 3,
        }
    }
}

/// One operator warning for a claim taken past the configured daily attempt
/// threshold. The UTC day and attempt ordinal come from the returned lease's
/// generation, written under the Claims lock, so the notice reflects the
/// authoritative count rather than a separate read. Free-form release reasons
/// stay out of it: no approved surface guarantees scrubbed output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptAdvisory {
    /// The attempt ordinal just reserved.
    pub attempt: u32,
    /// The configured advisory threshold; zero warns from the first attempt.
    pub limit: u32,
    /// The UTC day window that owns the ordinal.
    pub utc_day: u64,
}

impl AttemptAdvisory {
    /// The claim passed the threshold exactly when the reserved ordinal is past
    /// the limit, so a limit of zero warns from the first attempt.
    pub fn at(lease: &Lease, policy: &Policy) -> Option<Self> {
        let (utc_day, attempt) = lease.generation;
        (attempt > policy.max_attempts_per_day).then(|| Self {
            attempt,
            limit: policy.max_attempts_per_day,
            utc_day,
        })
    }

    /// One warning line for the claim of `number`.
    pub fn line(&self, number: u64) -> String {
        let start = self.utc_day.saturating_mul(86_400);
        let end = start.saturating_add(86_400);
        format!(
            "board item {number}: advisory attempt {} of {} in the UTC window {start}..{end}",
            self.attempt, self.limit
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Entry {
    pub number: u64,
    /// PR head, or frozen issue-source revision for BuildIssue/WaitIssue. Never a fabricated PR.
    pub head: String,
    pub release_pinned: bool,
    pub action: Action,
    pub lease: Option<Lease>,
    pub attempts_today: u32,
    pub eligible: bool,
}

impl Entry {
    fn priority(&self) -> (bool, Action, u64) {
        (!self.release_pinned, self.action, self.number)
    }
}

/// Maximum open pull requests in a complete repository board observation.
pub const MAX_OPEN_PULL_REQUESTS: usize = 1_000;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Board {
    pub observed_at: u64,
    pub entries: Vec<Entry>,
}

impl Board {
    /// `now` and lease expiry are Unix seconds; attempt days are UTC days since the epoch.
    /// State is read only. An executor must atomically reserve an attempt and lease before work.
    pub fn read(
        github: &mut impl GitHub,
        policy: &Policy,
        state: &BTreeMap<u64, AttemptState>,
        now: u64,
    ) -> crate::Result<Self> {
        Self::read_with_review_policy(
            github,
            policy,
            state,
            now,
            &review::Policy::default(),
            &BTreeMap::new(),
        )
        .map(|(board, _)| board)
    }

    /// The caller supplies trusted evidence for this repository, keyed by PR number.
    /// Evidence from a different head cannot qualify a PR or produce a review request.
    /// The returned plans are read-only. Dispatch requires a durable request reservation.
    pub fn read_with_review_policy(
        github: &mut impl GitHub,
        policy: &Policy,
        state: &BTreeMap<u64, AttemptState>,
        now: u64,
        review_policy: &review::Policy<'_>,
        evidence: &BTreeMap<u64, review::Facts<'_>>,
    ) -> crate::Result<(Self, BTreeMap<u64, review::Plan>)> {
        let mut board = Self {
            observed_at: now,
            entries: vec![],
        };
        let mut review_plans = BTreeMap::new();
        if !policy.enabled {
            return Ok((board, review_plans));
        }
        let mut seen = BTreeSet::new();
        let snapshot = github.snapshot()?;
        for pr in snapshot.pull_requests {
            if pr.number == 0 || pr.head.is_empty() || !seen.insert(pr.number) {
                return Err("invalid or duplicate pull request in board snapshot".into());
            }
            let facts = evidence.get(&pr.number).filter(|facts| facts.head == pr.head);
            let review = review_policy.enabled.then(|| {
                let Some(facts) = facts else {
                    return review::Decision::Wait;
                };
                let mut decision = review_policy.decide(&review::Facts {
                    required_ci: pr.required_ci,
                    draft: pr.draft,
                    unresolved_findings: pr.findings,
                    body: &pr.body,
                    ..*facts
                });
                if pr.mergeability != Mergeability::Mergeable {
                    if let review::Decision::Review { request, .. } = &mut decision {
                        request.clear();
                    }
                }
                decision
            });
            let action = if pr.findings {
                Action::FixFindings
            } else if pr.required_ci == RequiredCi::Failed {
                Action::FixRequiredCi
            } else if pr.mergeability == Mergeability::Conflicting {
                Action::ResolveConflict
            } else if matches!(review, Some(review::Decision::AddRemoves))
                || (review.is_none() && pr.removes_source && !review::has_removes_note(&pr.body))
            {
                Action::AddRemoves
            } else if pr.required_ci != RequiredCi::Passed {
                Action::WaitRequiredCi
            } else if pr.mergeability == Mergeability::Unknown {
                Action::WaitMergeability
            } else if pr.draft
                && (!review_policy.enabled
                    || facts.is_some_and(|facts| {
                        facts.review_admitted
                            && !facts.changes.is_empty()
                            && facts.verified_patch_id().is_some()
                    }))
            {
                Action::PromoteDraft
            } else if matches!(&review, Some(review::Decision::Review { outstanding, .. }) if outstanding.is_empty())
                || (review.is_none() && pr.reviewed)
            {
                Action::Ready
            } else {
                Action::WaitReview
            };
            if let Some(decision) = review {
                review_plans.insert(
                    pr.number,
                    review::Plan::new(
                        pr.number,
                        &pr.head,
                        facts.and_then(review::Facts::verified_patch_id),
                        decision,
                    ),
                );
            }
            board.entries.push(intake::entry(
                pr.number,
                pr.head,
                pr.release_pinned,
                action,
                state.get(&pr.number),
                now,
            ));
        }
        for issue in snapshot.issues {
            if issue.number == 0 || issue.revision.is_empty() || !seen.insert(issue.number) {
                return Err("invalid or duplicate issue in board snapshot".into());
            }
            let action = if issue.approved && !issue.has_open_pull_request {
                Action::BuildIssue
            } else {
                Action::WaitIssue
            };
            board.entries.push(intake::entry(
                issue.number,
                issue.revision,
                issue.release_pinned,
                action,
                state.get(&issue.number),
                now,
            ));
        }
        board.entries.sort_by_key(Entry::priority);
        Ok((board, review_plans))
    }
}
