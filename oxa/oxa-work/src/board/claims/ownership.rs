//! Opt-in item admission; trusted frontends supply host identity and complete work evidence.
use super::*;
use crate::board::assignment::Worker;

pub enum ExistingWork {
    PullRequest { number: u64, owner: String },
    Branch { name: String, owner: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemLease {
    pub lease: Lease,
    pub host_id: String,
    pub expires_at: u64,
}

impl Claims {
    pub fn claim_item_for(
        &self,
        entry: &Entry,
        policy: &Policy,
        worker: &Worker,
        host_id: &str,
        timing: (u64, u64),
        _observe: impl FnOnce(u64) -> Result<Option<ExistingWork>>,
    ) -> Result<Option<ItemLease>> {
        self.claim_for(entry, policy, worker, timing.0, timing.1)
            .map(|lease| {
                lease.map(|lease| ItemLease {
                    expires_at: lease.expires_at,
                    lease,
                    host_id: host_id.into(),
                })
            })
    }
}
