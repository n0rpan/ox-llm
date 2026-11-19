//! Lead "stuck" signals (#1575). A pure derivation from worker state and merge age:
//! no clock reads, no file or network access, no actions. A signal tells the lead that
//! work is not moving; resolving or escalating it is the lead pass's job, not this module's.

const MINUTE_MS: u64 = 60_000;

/// Owner default when `oxa.lead.*` sets nothing: 60 minutes without a merge while green PRs wait.
pub const DEFAULT_MERGE_STALL_MINUTES: u64 = 60;

/// Owner-configurable thresholds. The caller reads the setting and passes it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// Minutes without a merge while green PRs wait. Zero disables the merge-stall signal.
    pub merge_stall_minutes: u64,
}

impl Thresholds {
    /// `None` means the owner set no value, so the default applies.
    pub fn from_settings(merge_stall_minutes: Option<u64>) -> Self {
        Self {
            merge_stall_minutes: merge_stall_minutes.unwrap_or(DEFAULT_MERGE_STALL_MINUTES),
        }
    }
}

impl Default for Thresholds {
    fn default() -> Self {
        Self::from_settings(None)
    }
}

/// One worker's state as the runner reports it. The caller derives the booleans from facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerState {
    pub task: String,
    /// A local acceptance exists and no publication is recorded for it yet.
    pub publish_pending: bool,
    /// The last GitHub call for this worker was refused as unauthenticated (HTTP 401).
    pub auth_failed: bool,
    /// The worker is idle while the queue still holds work for it.
    pub idle_with_work_left: bool,
}

/// Repository-level merge state. Ages are supplied by the caller's clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeState {
    /// Open, non-merged PRs whose checks are green.
    pub green_prs: u32,
    /// Milliseconds since the last merge; `None` when no merge is known (never flagged).
    pub since_last_merge_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signal {
    PublishPending { task: String },
    AuthFailure { task: String },
    IdleWithWork { task: String },
    MergeStalled { green_prs: u32, minutes: u64 },
}

/// Signals in a fixed order: per worker (publish, auth, idle) in input order, then the merge stall.
pub fn derive(workers: &[WorkerState], merge: MergeState, thresholds: Thresholds) -> Vec<Signal> {
    let mut signals = Vec::new();
    for worker in workers {
        if worker.publish_pending {
            signals.push(Signal::PublishPending {
                task: worker.task.clone(),
            });
        }
        if worker.auth_failed {
            signals.push(Signal::AuthFailure {
                task: worker.task.clone(),
            });
        }
        if worker.idle_with_work_left {
            signals.push(Signal::IdleWithWork {
                task: worker.task.clone(),
            });
        }
    }
    if let Some(minutes) = merge_stall(merge, thresholds) {
        signals.push(Signal::MergeStalled {
            green_prs: merge.green_prs,
            minutes,
        });
    }
    signals
}

fn merge_stall(merge: MergeState, thresholds: Thresholds) -> Option<u64> {
    let minutes = thresholds.merge_stall_minutes;
    if minutes == 0 || merge.green_prs == 0 {
        return None;
    }
    let since = merge.since_last_merge_ms?;
    (since >= minutes.saturating_mul(MINUTE_MS)).then_some(minutes)
}
