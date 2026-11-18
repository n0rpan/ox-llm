//! Opt-in item admission; trusted frontends supply host identity and complete work evidence.
use super::*;
use crate::board::assignment::{AssignmentPolicy, ManualAuthority, Worker};

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

/// Renewable fence in the existing row; the original Lease and NativeBinding never change.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ItemOwner {
    pub host_id: String,
    pub expires_at: u64,
}

impl Saved {
    fn item_lease(&self) -> Option<ItemLease> {
        let owner = self.data.item_owner.as_ref()?;
        Some(ItemLease {
            lease: self.state.lease.clone()?,
            host_id: owner.host_id.clone(),
            expires_at: owner.expires_at,
        })
    }

    pub(super) fn lease_expiry(&self) -> Option<u64> {
        self.state.lease.as_ref().map(|lease| {
            self.data
                .item_owner
                .as_ref()
                .map_or(lease.expires_at, |owner| owner.expires_at)
        })
    }
}

impl Claims {
    /// Explicit opt-in before any new work. The trusted observer must check complete, fresh
    /// PR and branch evidence for this repository/item or fail. It runs under the Claims lock
    /// and must not call Claims. All hosts must use this same protected authority.
    pub fn claim_item_for(
        &self,
        entry: &Entry,
        policy: &Policy,
        worker: &Worker,
        host_id: &str,
        timing: (u64, u64),
        observe: impl FnOnce(u64) -> Result<Option<ExistingWork>>,
    ) -> Result<Option<ItemLease>> {
        if !valid_worker_id(host_id) {
            return Err("authenticated host identity required".into());
        }
        self.claim_checked(entry, policy, &worker.id, timing, Some(host_id), |row| {
            if let Some(existing) = observe(entry.number)? {
                let (owner, kind) = match existing {
                    ExistingWork::PullRequest { number, owner } if number > 0 => (owner, "open PR"),
                    ExistingWork::Branch { name, owner } if !name.is_empty() => (owner, "branch"),
                    _ => return Err("invalid existing work evidence".into()),
                };
                if !valid_worker_id(&owner) {
                    return Err("existing work requires an identified owner".into());
                }
                return Err(format!("item {}: {kind} owned by {owner}", entry.number));
            }
            Ok(row
                .data
                .policy
                .get_or_insert_with(AssignmentPolicy::default)
                .allows(worker, entry.action))
        })
        .map(|lease| {
            lease.map(|lease| ItemLease {
                expires_at: lease.expires_at,
                lease,
                host_id: host_id.into(),
            })
        })
    }

    /// Both identities come from authenticated host/worker registration, never packet JSON.
    /// Renewal CAS includes the full original lease, host and most recent expiry.
    pub fn renew_item_for(
        &self,
        number: u64,
        worker_id: &str,
        host_id: &str,
        receipt: &ItemLease,
        now: u64,
        ttl: u64,
    ) -> Result<Option<ItemLease>> {
        if worker_id != receipt.lease.holder || host_id != receipt.host_id {
            return Err("authenticated item owner and host required".into());
        }
        let _lock = self.lock(number)?;
        let mut row = self.load(number)?;
        if row.item_lease().as_ref() != Some(receipt)
            || receipt.lease.claimed_at.is_none_or(|at| now < at)
            || now >= receipt.expires_at
        {
            return Ok(None);
        }
        let expires_at = now
            .checked_add(ttl)
            .filter(|until| *until > receipt.expires_at)
            .ok_or("renewal must extend the live expiry without overflow")?;
        let owner = row.data.item_owner.as_mut().ok_or("missing item owner")?;
        owner.expires_at = expires_at;
        self.save(number, row, true)?;
        self.mark(number)?;
        Ok(Some(ItemLease {
            expires_at,
            ..receipt.clone()
        }))
    }

    /// Expiry alone is insufficient. Trusted lead/owner verifies this exact holder/host
    /// is gone under the reservation lock; verification must not call back into Claims.
    pub fn reclaim_item(
        &self,
        number: u64,
        authority: Option<ManualAuthority>,
        receipt: &ItemLease,
        now: u64,
        verify_gone: impl FnOnce(&ItemLease) -> Result<bool>,
    ) -> Result<bool> {
        if authority.is_none() {
            return Err("owner or lead reclaim required".into());
        }
        self.end_lease(number, now, "expired holder verified gone", |row| {
            if row.item_lease().as_ref() != Some(receipt) || now < receipt.expires_at {
                return Ok(false);
            }
            verify_gone(receipt)
        })
    }
}
