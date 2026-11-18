//! Immutable native attempt receipts in the existing protected assignment row.
use super::*;

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeBinding {
    pub number: u64,
    pub key: String,
    pub lease: Lease,
    pub native_task_id: String,
    pub branch: String,
}

impl NativeBinding {
    /// Inputs come from the trusted prepared packet and the original claim receipt.
    pub fn new(number: u64, key: &str, lease: &Lease, branch: &str) -> Result<Self> {
        let binding = Self {
            number,
            key: key.into(),
            lease: lease.clone(),
            native_task_id: format!("board-{key}-{}-{}", lease.generation.0, lease.generation.1),
            branch: branch.into(),
        };
        if !binding.valid() {
            return Err("invalid native attempt binding".into());
        }
        Ok(binding)
    }

    pub(super) fn valid(&self) -> bool {
        self.number > 0
            && self.key.len() == 64
            && self
                .key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            && valid_worker_id(&self.lease.holder)
            && self.lease.generation.1 > 0
            && self.lease.claimed_at.is_some_and(|at| at < self.lease.expires_at)
            && self.native_task_id
                == format!(
                    "board-{}-{}-{}",
                    self.key, self.lease.generation.0, self.lease.generation.1
                )
            && crate::valid_task_id(&self.native_task_id)
            && !self.branch.is_empty()
            && self.branch.len() <= 256
            && !self.branch.starts_with(['-', '/'])
            && !self.branch.ends_with(['/', '.'])
            && !self.branch.contains([' ', '~', '^', ':', '?', '*', '[', '\\'])
            && !self.branch.contains("..")
            && !self.branch.contains("@{")
            && !self.branch.chars().any(char::is_control)
            && self
                .branch
                .split('/')
                .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
    }
}

impl Claims {
    /// Guard an append for a trusted frontend's original native binding. Reassignment
    /// and release use the same lock. The operation must only perform the append
    /// and must not call back into Claims.
    pub fn with_live_native_lease<T>(
        &self,
        binding: &NativeBinding,
        now: impl FnOnce() -> u64,
        operation: impl FnOnce(&Lease) -> T,
    ) -> Result<Option<T>> {
        if !binding.valid() || Self::open(&self.root)?.uid != self.uid {
            return Err("invalid native attempt or board claim directory".into());
        }
        let lock = io(File::open(&self.root))?;
        io(lock.lock())?;
        let row = self.load(binding.number)?;
        let Some(lease) = row.state.lease.as_ref() else {
            return Ok(None);
        };
        if row.data.native_binding.as_ref() != Some(binding)
            || lease.holder != binding.lease.holder
            || lease.generation != binding.lease.generation
        {
            return Ok(None);
        }
        let now = now();
        if !lease.claimed_at.is_some_and(|start| start <= now) || now >= lease.expires_at {
            return Ok(None);
        }
        Ok(Some(operation(lease)))
    }

    /// Persist before queue publication. Identity is authenticated by the trusted frontend.
    /// Failure leaves the original reservation charged and must prevent the ready hardlink.
    pub fn bind_native(&self, worker_id: &str, binding: &NativeBinding) -> Result<bool> {
        if !binding.valid() || worker_id != binding.lease.holder {
            return Err("authenticated native attempt binding required".into());
        }
        let _lock = self.lock(binding.number)?;
        let mut row = self.load(binding.number)?;
        if row.state.lease.as_ref() != Some(&binding.lease) {
            return Ok(false);
        }
        if let Some(existing) = &row.data.native_binding {
            return Ok(existing == binding);
        }
        for (number, other) in self.rows_locked()? {
            if number != binding.number
                && other.data.native_binding.as_ref().is_some_and(|existing| {
                    existing.native_task_id == binding.native_task_id
                        || (other.state.lease.is_some() && existing.branch == binding.branch)
                })
            {
                return Ok(false);
            }
        }
        row.data.native_binding = Some(binding.clone());
        self.save(binding.number, row, true)?;
        self.mark(binding.number)?;
        Ok(true)
    }

    /// Match the admitted native identity, retaining the original full lease after release.
    /// This read is not completion authority; release still requires trusted terminal evidence.
    pub fn native_binding(&self, native_task_id: &str, branch: &str) -> Result<Option<NativeBinding>> {
        let mut found = None;
        for row in self.rows()?.into_values() {
            if let Some(binding) = row.data.native_binding {
                if binding.native_task_id == native_task_id && binding.branch == branch {
                    if found.is_some() {
                        return Err("ambiguous native attempt binding".into());
                    }
                    found = Some(binding);
                }
            }
        }
        Ok(found)
    }
}
