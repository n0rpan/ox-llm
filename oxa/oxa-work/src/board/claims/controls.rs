//! Authenticated controls share the exact record and lock used by claim admission.
use super::*;
use crate::board::assignment::{AssignmentPolicy, Control, ManualAuthority, Worker};
use crate::board::{Board, GitHub};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssignmentData {
    pub revision: u64,
    pub policy: Option<AssignmentPolicy>,
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
        Ok(AssignmentBoard {
            board: Board::read(github, policy, &states, now)?,
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
}
