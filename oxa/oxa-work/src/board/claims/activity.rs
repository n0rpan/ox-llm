//! Trusted per-lease observations; silence never releases ownership or establishes liveness.
use super::*;
use crate::{CheckpointKind, Record, Step};

pub enum LeaseSignal<'a> {
    /// A trusted frontend observed its exact managed child, not worker-provided output.
    Heartbeat,
    /// Synced result and its runner-owned original launch revision, never a child-provided ID.
    Milestone(&'a Record, u64),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerifiedMilestone {
    pub launch_revision: u64,
    pub revision: u64,
    pub at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LeaseActivity {
    pub generation: (u64, u32),
    pub heartbeat_at: Option<u64>,
    pub last_action: Option<VerifiedMilestone>,
}

impl LeaseActivity {
    pub(super) fn valid(&self, state: &AttemptState) -> bool {
        self.generation == (state.utc_day, state.attempts)
            && self.generation.1 > 0
            && self.last_action.as_ref().is_none_or(|action| {
                action.launch_revision > 0 && action.launch_revision < action.revision && action.at_ms > 0
            })
            && state.lease.as_ref().is_none_or(|lease| {
                lease.claimed_at.is_some_and(|start| {
                    self.heartbeat_at.is_none_or(|at| at >= start)
                        && self
                            .last_action
                            .as_ref()
                            .is_none_or(|action| action.at_ms / 1000 >= start)
                })
            })
    }
}

impl Assignment {
    /// Timeout is opt-in. Heartbeat and observation time never reset the action deadline.
    /// A true result is an assessment, not permission to bypass verified expiry/gone reclaim.
    pub fn stalled(&self, now: u64, silence: Option<u64>) -> Result<bool> {
        let Some(silence) = silence else { return Ok(false) };
        if silence == 0 {
            return Err("silence timeout must be positive".into());
        }
        let Some(lease) = self
            .state
            .lease
            .as_ref()
            .filter(|_| self.data.item_owner.is_some())
        else {
            return Ok(false);
        };
        let last = self
            .data
            .lease_activity
            .as_ref()
            .and_then(|activity| activity.last_action.as_ref())
            .map(|action| action.at_ms)
            .or_else(|| lease.claimed_at.and_then(|at| at.checked_mul(1000)))
            .ok_or("item has no action clock")?;
        if now < last / 1000 {
            return Err("item action clock moved backwards".into());
        }
        let deadline = silence
            .checked_mul(1000)
            .and_then(|timeout| last.checked_add(timeout))
            .ok_or("silence deadline overflow")?;
        let now_ms = now.checked_mul(1000).ok_or("item action clock overflow")?;
        if let Some(wait) = &self.data.dependency {
            return Ok(now >= wait.deadline);
        }
        Ok(now_ms >= deadline)
    }
}

impl Claims {
    /// Only the authenticated runtime observer may supply signals. The current receipt,
    /// original binding and host are checked under the existing Claims lock. No callbacks
    /// or task/checkpoint lock acquisition occur here; callers must not invert those locks.
    pub fn observe_item_for(
        &self,
        worker_id: &str,
        host_id: &str,
        binding: &NativeBinding,
        receipt: &ItemLease,
        now: u64,
        signal: LeaseSignal<'_>,
    ) -> Result<bool> {
        let Some((_lock, mut row)) = self.active_item_for(worker_id, host_id, binding, receipt, now)? else {
            return Ok(false);
        };
        let activity = row.data.lease_activity.get_or_insert(LeaseActivity {
            generation: receipt.lease.generation,
            heartbeat_at: None,
            last_action: None,
        });
        match signal {
            LeaseSignal::Heartbeat => {
                if activity.heartbeat_at.is_some_and(|at| now < at) {
                    return Err("item heartbeat clock moved backwards".into());
                }
                if activity.heartbeat_at == Some(now) {
                    return Ok(false);
                }
                activity.heartbeat_at = Some(now);
            }
            LeaseSignal::Milestone(record, launch_revision) => {
                if record.id != binding.native_task_id
                    || record.branch != binding.branch
                    || record.checkpoint_kind != CheckpointKind::WorkerFinished
                    || !matches!(record.step, Step::Acceptance | Step::Recheck)
                    || record.worker.is_none()
                    || record.checkpoint_revision == 0
                {
                    return Ok(false);
                }
                if launch_revision == 0 || launch_revision >= record.checkpoint_revision {
                    return Err("invalid original process launch revision".into());
                }
                if activity.last_action.as_ref().is_some_and(|last| {
                    record.checkpoint_revision <= last.revision || launch_revision <= last.revision
                }) {
                    return Ok(false);
                }
                if record.checkpoint_at_ms == 0
                    || record.checkpoint_at_ms / 1000 > now
                    || record.checkpoint_at_ms / 1000 < receipt.lease.claimed_at.unwrap()
                    || activity
                        .last_action
                        .as_ref()
                        .is_some_and(|last| record.checkpoint_at_ms < last.at_ms)
                {
                    return Err("invalid durable action clock".into());
                }
                activity.last_action = Some(VerifiedMilestone {
                    launch_revision,
                    revision: record.checkpoint_revision,
                    at_ms: record.checkpoint_at_ms,
                });
            }
        }
        self.save(binding.number, row, true)?;
        self.mark(binding.number)?;
        Ok(true)
    }
}

#[cfg(test)]
pub(super) mod tests;
