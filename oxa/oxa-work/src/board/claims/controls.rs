//! Authenticated controls share the exact record and lock used by claim admission.
use super::*;
use crate::board::assignment::{AssignmentPolicy, Control, ManualAuthority, Worker};
use crate::board::{Board, GitHub};

mod leases;
pub use leases::ReleaseRecord;

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssignmentData {
    pub revision: u64,
    pub policy: Option<AssignmentPolicy>,
    #[serde(default)]
    pub import_hold: bool,
    #[serde(default)]
    pub last_release: Option<ReleaseRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_admission: Option<QueueAdmission>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum QueueAdmission {
    Reserved,
    Retained { key: String, lease: Lease },
}

fn valid_queue_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl QueueAdmission {
    pub(super) fn matches_lease(&self, current: Option<&Lease>) -> bool {
        match self {
            Self::Reserved => current.is_some(),
            Self::Retained { key, lease } => valid_queue_key(key) && current == Some(lease),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Assignment {
    pub number: u64,
    pub state: AttemptState,
    pub data: AssignmentData,
}

impl Assignment {
    pub fn assignee(&self, now: u64) -> Option<&str> {
        self.state
            .lease
            .as_ref()
            .filter(|lease| lease.expires_at > now)
            .map(|lease| lease.holder.as_str())
    }
}

#[derive(Clone, Serialize)]
pub struct AssignmentBoard {
    pub board: Board,
    pub assignments: BTreeMap<u64, Assignment>,
    pub workers: Vec<Worker>,
}

impl AssignmentBoard {
    pub fn candidates(&self, worker_id: &str) -> Result<Vec<u64>> {
        let worker = self
            .workers
            .iter()
            .find(|worker| worker.id == worker_id)
            .ok_or("unknown registered worker")?;
        let mut ranked = vec![];
        for entry in &self.board.entries {
            let policy = self
                .assignments
                .get(&entry.number)
                .and_then(|row| row.data.policy.as_ref())
                .cloned()
                .unwrap_or_default();
            if let Some(rank) = policy.rank(worker, entry) {
                ranked.push((rank, entry.number));
            }
        }
        ranked.sort_by_key(|(rank, _)| *rank);
        Ok(ranked.into_iter().map(|(_, number)| number).collect())
    }
}

impl Claims {
    /// The catalog comes from existing authenticated host registration, never GitHub labels.
    /// One complete GitHub read serves every worker in this immutable query result.
    pub fn assignment_board(
        &self,
        github: &mut impl GitHub,
        policy: &Policy,
        workers: &[Worker],
        now: u64,
    ) -> Result<AssignmentBoard> {
        let assignments = if policy.enabled {
            self.assignments()?
        } else {
            BTreeMap::new()
        };
        let states = assignments
            .iter()
            .map(|(number, row)| (*number, row.state.clone()))
            .collect();
        let mut ids = std::collections::BTreeSet::new();
        if workers
            .iter()
            .any(|worker| !valid_worker_id(&worker.id) || !ids.insert(&worker.id))
        {
            return Err("invalid or duplicate worker catalog".into());
        }
        let mut board = Board::read(github, policy, &states, now)?;
        for entry in &mut board.entries {
            if assignments
                .get(&entry.number)
                .is_some_and(|row| row.data.import_hold || row.data.queue_admission.is_some())
            {
                entry.eligible = false;
            }
        }
        Ok(AssignmentBoard {
            board,
            assignments,
            workers: workers.to_vec(),
        })
    }
    /// Full private query projection, not a second facts database.
    pub fn assignments(&self) -> Result<BTreeMap<u64, Assignment>> {
        Ok(self
            .rows()?
            .into_iter()
            .map(|(number, row)| {
                (
                    number,
                    Assignment {
                        number,
                        state: row.state,
                        data: row.data,
                    },
                )
            })
            .collect())
    }

    /// The authority is supplied by trusted user/lead authentication, not request JSON.
    pub fn control(
        &self,
        number: u64,
        authority: Option<ManualAuthority>,
        expected_revision: u64,
        change: Control,
    ) -> Result<()> {
        if authority.is_none() {
            return Err("owner or lead control required".into());
        }
        let _lock = self.lock(number)?;
        let mut row = self.load(number)?;
        if row.data.revision != expected_revision {
            return Err("assignment changed; refresh it".into());
        }
        let mut policy = row.data.policy.take().unwrap_or_default();
        policy.control(authority, change)?;
        row.data.policy = Some(policy);
        let replacing = self.established(number)?;
        self.save(number, row, replacing)?;
        self.mark(number)
    }

    /// Worker declarations come from the authenticated registration frontend.
    /// Manual state is reread under the reservation lock; stale caller snapshots cannot bypass it.
    pub fn claim_for(
        &self,
        entry: &Entry,
        policy: &Policy,
        worker: &Worker,
        now: u64,
        ttl: u64,
    ) -> Result<Option<Lease>> {
        self.claim_checked(entry, policy, &worker.id, (now, ttl), |row| {
            let controls = row.data.policy.get_or_insert_with(AssignmentPolicy::default);
            Ok(controls.allows(worker, entry.action))
        })
    }

    /// Helper admission checks idleness and reserves under the SAME directory lock.
    /// The target must have validated Saved state; transient lock markers are not bootstrap facts.
    /// Every outstanding target/worker lease, including expired work, requires fenced completion.
    /// Worker identity and roles must come from the trusted frontend, as with `claim_for`.
    /// Generic manual reservations retain their separate `assign`/`claim_for` semantics.
    pub fn claim_idle_for(
        &self,
        entry: &Entry,
        policy: &Policy,
        worker: &Worker,
        now: u64,
        ttl: u64,
    ) -> Result<Option<Lease>> {
        self.claim_checked(entry, policy, &worker.id, (now, ttl), |row| {
            if !self.established(entry.number)?
                || row.state.lease.is_some()
                || self.rows_locked()?.values().any(|saved| {
                    saved
                        .state
                        .lease
                        .as_ref()
                        .is_some_and(|lease| lease.holder == worker.id)
                })
            {
                return Ok(false);
            }
            let controls = row.data.policy.get_or_insert_with(AssignmentPolicy::default);
            if !controls.allows(worker, entry.action) {
                return Ok(false);
            }
            row.data.queue_admission = Some(QueueAdmission::Reserved);
            Ok(true)
        })
    }
}
