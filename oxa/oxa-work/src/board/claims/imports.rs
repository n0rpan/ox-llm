//! Bootstrap is restartable, refuses conflicting native facts, and never refunds attempts.
use super::*;
use crate::board::{assignment::ManualAuthority, ImportState};

impl Claims {
    /// Caller keeps activation OFF until the entire authoritative batch is imported and verified.
    /// A partial I/O failure can be retried; an established, differing state is never overwritten.
    pub fn import_snapshot(
        &self,
        authority: Option<ManualAuthority>,
        incoming: &BTreeMap<u64, ImportState>,
        now: u64,
    ) -> Result<()> {
        if authority.is_none() {
            return Err("owner or lead import required".into());
        }
        for (number, item) in incoming {
            let state = &item.state;
            if *number == 0
                || state.utc_day > now / 86_400
                || state.lease.as_ref().is_some_and(|l| {
                    state.attempts == 0
                        || !valid_worker_id(&l.holder)
                        || l.generation != (state.utc_day, state.attempts)
                        || l.claimed_at.is_none_or(|at| at > now || at >= l.expires_at)
                })
            {
                return Err("invalid imported claim state".into());
            }
        }
        if Self::open(&self.root)?.uid != self.uid {
            return Err("invalid import directory".into());
        }
        let lock = io(File::open(&self.root))?;
        io(lock.lock())?;
        let mut prepared = vec![];
        for (number, item) in incoming {
            let mut row = self.load(*number)?;
            let exists = self.established(*number)?;
            if exists && row.state != item.state {
                return Err("import conflicts with established claims; reconciliation required".into());
            }
            let held = item.excluded || item.state.lease.as_ref().is_some_and(|l| l.expires_at > now);
            let changed = !exists || (held && !row.data.import_hold);
            row.state = item.state.clone();
            row.data.import_hold |= held;
            prepared.push((*number, row, exists, changed));
        }
        for (number, row, exists, changed) in prepared {
            if changed {
                self.save(number, row, exists)?;
            }
            self.mark(number)?;
        }
        Ok(())
    }

    /// Only after the trusted frontend verifies preserved queued/active work has ended.
    pub fn clear_import_hold(
        &self,
        number: u64,
        authority: Option<ManualAuthority>,
        expected_revision: u64,
    ) -> Result<()> {
        if authority.is_none() {
            return Err("owner or lead reconciliation required".into());
        }
        let _lock = self.lock(number)?;
        let mut row = self.load(number)?;
        if row.data.revision != expected_revision {
            return Err("assignment changed; refresh it".into());
        }
        if !row.data.import_hold {
            return Ok(());
        }
        row.data.import_hold = false;
        self.save(number, row, true)?;
        self.mark(number)
    }
}
