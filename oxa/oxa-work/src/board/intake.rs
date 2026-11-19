//! Typed intake and conservative bootstrap data; no acquisition, partitioner or second store.
use super::*;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Issue {
    pub number: u64,
    pub revision: String,
    pub release_pinned: bool,
    /// Trusted intake approval, never inferred from arbitrary issue text.
    pub approved: bool,
    pub has_open_pull_request: bool,
}
/// A PR whose evidence could not be read completely. It is shown as one row and is never eligible.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Incomplete {
    pub number: u64,
    pub head: String,
    pub release_pinned: bool,
    /// Names the GitHub connection that exceeded its read page; logged and shown with the row.
    pub reason: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Snapshot {
    pub pull_requests: Vec<PullRequest>,
    pub issues: Vec<Issue>,
    /// Items the adapter could not evaluate completely; the rest of the snapshot stays usable.
    #[serde(default)]
    pub incomplete: Vec<Incomplete>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportState {
    pub state: AttemptState,
    /// Queued OR active legacy work excludes native admission until explicitly reconciled.
    pub excluded: bool,
}
pub(super) fn entry(
    number: u64,
    head: String,
    release_pinned: bool,
    action: Action,
    saved: Option<&AttemptState>,
    policy: &Policy,
    now: u64,
) -> Entry {
    let attempts_today = saved
        .filter(|s| s.utc_day >= now / 86_400)
        .map_or(0, |s| s.attempts);
    let lease = saved.and_then(|s| s.lease.clone());
    let eligible = !matches!(
        action,
        Action::WaitReview
            | Action::WaitRequiredCi
            | Action::WaitMergeability
            | Action::WaitIssue
            | Action::EvidenceIncomplete
    ) && attempts_today < policy.max_attempts_per_day
        && lease.as_ref().is_none_or(|l| l.expires_at <= now);
    Entry {
        number,
        head,
        release_pinned,
        action,
        lease,
        attempts_today,
        eligible,
        evidence_incomplete: None,
    }
}

/// One row for an incomplete PR: visible with its reason, and never eligible for any action.
pub(super) fn incomplete_entry(
    item: Incomplete,
    saved: Option<&AttemptState>,
    policy: &Policy,
    now: u64,
) -> Entry {
    let mut entry = entry(
        item.number,
        item.head,
        item.release_pinned,
        Action::EvidenceIncomplete,
        saved,
        policy,
        now,
    );
    entry.eligible = false;
    entry.evidence_incomplete = Some(item.reason);
    entry
}
