//! Lease changes use the same admission accounting, revision and private record as controls.
use super::*;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRecord {
    pub holder: String,
    pub released_at: u64,
    pub reason: String,
}

impl Claims {
    /// Manual assignment cannot bypass a pin, role requirement, live lease or attempt cap.
    pub fn assign(
        &self,
        entry: &Entry,
        policy: &Policy,
        authority: Option<ManualAuthority>,
        expected_revision: u64,
        worker: &Worker,
        now: u64,
        ttl: u64,
    ) -> Result<Option<Lease>> {
        if authority.is_none() {
            return Err("owner or lead assignment required".into());
        }
        self.claim_checked(entry, policy, &worker.id, (now, ttl), None, |row| {
            if row.data.revision != expected_revision {
                return Err("assignment changed; refresh it".into());
            }
            Ok(row
                .data
                .policy
                .get_or_insert_with(AssignmentPolicy::default)
                .allows(worker, entry.action))
        })
    }

    /// The trusted worker frontend supplies identity; knowing a lease is not authority to release it.
    pub fn release_for(
        &self,
        number: u64,
        worker_id: &str,
        lease: &Lease,
        now: u64,
        reason: &str,
    ) -> Result<bool> {
        if !valid_worker_id(worker_id) || worker_id != lease.holder {
            return Err("lease holder authentication required".into());
        }
        self.end_lease(number, now, reason, |row| {
            Ok(row.state.lease.as_ref() == Some(lease))
        })
    }

    pub fn unassign(
        &self,
        number: u64,
        authority: Option<ManualAuthority>,
        expected_revision: u64,
        now: u64,
        reason: &str,
    ) -> Result<bool> {
        if authority.is_none() {
            return Err("owner or lead unassignment required".into());
        }
        self.end_lease(number, now, reason, |row| {
            if row.data.item_owner.is_some() {
                return Err("host-bound work requires authenticated completion or verified reclaim".into());
            }
            if row.data.revision != expected_revision {
                return Err("assignment changed; refresh it".into());
            }
            Ok(true)
        })
    }

    pub(in crate::board::claims) fn end_lease(
        &self,
        number: u64,
        now: u64,
        reason: &str,
        authorize: impl FnOnce(&super::super::Saved) -> Result<bool>,
    ) -> Result<bool> {
        let reason = reason.trim();
        if reason.is_empty() || reason.len() > 512 || reason.chars().any(char::is_control) {
            return Err("release requires a bounded single-line reason".into());
        }
        let _lock = self.lock(number)?;
        let mut row = self.load(number)?;
        if !authorize(&row)? {
            return Ok(false);
        }
        let Some(lease) = row.state.lease.take() else {
            return Ok(false);
        };
        if lease.claimed_at.is_none_or(|at| at > now) {
            return Err("board release clock moved backwards".into());
        }
        row.data.last_release = Some(ReleaseRecord {
            holder: lease.holder,
            released_at: now,
            reason: reason.into(),
        });
        self.save(number, row, true)?;
        self.mark(number)?;
        Ok(true)
    }
}
