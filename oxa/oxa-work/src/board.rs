//! Read-only GitHub board policy. Snapshot acquisition and durable claims belong to the caller.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

mod claims;
pub use claims::{Assignment, AssignmentBoard, AssignmentData, Claims, ReleaseRecord};
pub mod assignment;
mod intake;
pub mod review;
pub use intake::{ImportState, Issue, Snapshot};

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
        let mut board = Self {
            observed_at: now,
            entries: vec![],
        };
        if !policy.enabled {
            return Ok(board);
        }
        let mut seen = BTreeSet::new();
        let snapshot = github.snapshot()?;
        for pr in snapshot.pull_requests {
            if pr.number == 0 || pr.head.is_empty() || !seen.insert(pr.number) {
                return Err("invalid or duplicate pull request in board snapshot".into());
            }
            let action = if pr.findings {
                Action::FixFindings
            } else if pr.required_ci == RequiredCi::Failed {
                Action::FixRequiredCi
            } else if pr.mergeability == Mergeability::Conflicting {
                Action::ResolveConflict
            } else if pr.removes_source
                && !pr.body.lines().any(|line| {
                    line.trim()
                        .strip_prefix("Removes:")
                        .is_some_and(|note| !note.trim().is_empty())
                })
            {
                Action::AddRemoves
            } else if pr.required_ci != RequiredCi::Passed {
                Action::WaitRequiredCi
            } else if pr.mergeability == Mergeability::Unknown {
                Action::WaitMergeability
            } else if pr.draft {
                Action::PromoteDraft
            } else if pr.reviewed {
                Action::Ready
            } else {
                Action::WaitReview
            };
            board.entries.push(intake::entry(
                pr.number,
                pr.head,
                pr.release_pinned,
                action,
                state.get(&pr.number),
                policy,
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
                policy,
                now,
            ));
        }
        board
            .entries
            .sort_by_key(|e| (!e.release_pinned, e.action, e.number));
        Ok(board)
    }
}
