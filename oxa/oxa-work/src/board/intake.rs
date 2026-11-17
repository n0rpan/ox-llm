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
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Snapshot {
    pub pull_requests: Vec<PullRequest>,
    pub issues: Vec<Issue>,
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
        Action::WaitReview | Action::WaitRequiredCi | Action::WaitMergeability | Action::WaitIssue
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
    }
}
