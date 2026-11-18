//! Parked work retains item ownership; a deadline wakes a decision, never proves clearance.
use super::*;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Dependency {
    RequiredCi(u64),
    ReviewedHead { number: u64, head: String },
    WorkItem(u64),
    OwnerRead(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencyWait {
    pub dependency: Dependency,
    pub deadline: u64,
}

impl DependencyWait {
    pub(super) fn valid(&self, number: u64) -> bool {
        self.deadline > 0
            && match &self.dependency {
                Dependency::RequiredCi(run) => *run > 0,
                Dependency::ReviewedHead { number, head } => {
                    *number > 0 && super::super::queue::is_object_id(head)
                }
                Dependency::WorkItem(item) => *item > 0 && *item != number,
                Dependency::OwnerRead(id) => crate::valid_task_id(id),
            }
    }
}

impl Claims {
    /// The trusted observer verifies the exact dependency and safe quiescence before parking.
    /// It runs under Claims: never acquire task/repository
    /// locks or call Claims inside it. Heartbeat, worker text and a deadline are not proof.
    pub fn wait_for_dependency(
        &self,
        worker_id: &str,
        host_id: &str,
        binding: &NativeBinding,
        receipt: &ItemLease,
        now: u64,
        expected_revision: u64,
        wait: &DependencyWait,
        verify: impl FnOnce(&DependencyWait) -> Result<bool>,
    ) -> Result<bool> {
        let Some((_lock, mut row)) = self.active_item_for(worker_id, host_id, binding, receipt, now)? else {
            return Ok(false);
        };
        if row.data.revision != expected_revision {
            return Err("assignment changed; refresh it".into());
        }
        if !wait.valid(binding.number) || wait.deadline <= now {
            return Err("invalid dependency or future wake deadline required".into());
        }
        if row.data.dependency.as_ref() == Some(wait) || !verify(wait)? {
            return Ok(false);
        }
        row.data.dependency = Some(wait.clone());
        self.save(binding.number, row, true)?;
        self.mark(binding.number)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
