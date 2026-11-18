//! Queue-only consuming adapter; registration, prepared taskfiles and activation are user-owned.
use oxa_work::board::{assignment::Worker, *};
use oxa_work::{helper_queue, Result};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fs, path::Path};
/// Read-only acquisition and authoritative revision/action revalidation; no write capability.
pub trait Source: GitHub {
    fn revalidate(&mut self, entry: &Entry) -> Result<bool>;
    /// Only legacy adapters may report no host; native adapters must refuse it.
    fn host_id(&self) -> Result<Option<&str>> {
        Ok(None)
    }
    /// Revalidate source and complete existing-work facts under the Claims lock.
    /// The retained binding comes from that locked row, not a caller's cached snapshot.
    /// This callback must not call Claims; unavailable evidence must fail closed.
    fn existing_work(
        &mut self,
        _entry: &Entry,
        _retained: Option<&NativeBinding>,
    ) -> Result<Option<ExistingWork>> {
        Err("hosted admission requires complete existing-work evidence".into())
    }
}
/// Apply the same operator-selected item partition to BOTH PR and issue intake.
pub struct Partition {
    pub modulus: u64,
    pub residues: BTreeSet<u64>,
}
impl Partition {
    pub(crate) fn contains(&self, number: u64) -> bool {
        self.residues.contains(&(number % self.modulus))
    }

    pub(crate) fn validate(&self, max_attempts_per_day: u32) -> Result<()> {
        if self.modulus == 0
            || self.residues.is_empty()
            || self.residues.iter().any(|r| *r >= self.modulus)
            || max_attempts_per_day > 3
        {
            return Err("invalid board partition or attempt cap".into());
        }
        Ok(())
    }
}
struct Partitioned<'a, S>(&'a mut S, &'a Partition);
impl<S: GitHub> GitHub for Partitioned<'_, S> {
    fn snapshot(&mut self) -> Result<Snapshot> {
        let mut snapshot = self.0.snapshot()?;
        snapshot.pull_requests.retain(|pr| self.1.contains(pr.number));
        snapshot.issues.retain(|issue| self.1.contains(issue.number));
        Ok(snapshot)
    }
}
/// Existing bounded taskfile bytes, frozen to the exact candidate; never a new packet builder.
pub struct Packet<'a> {
    pub number: u64,
    pub revision: &'a str,
    pub action: Action,
    pub task: &'a str,
}
pub struct Receipt {
    pub number: u64,
    pub key: String,
    pub lease: Lease,
}

/// Authenticated queue target; default OFF. The operator-owned pause marker survives restarts.
pub struct Consumer<'a> {
    pub repository: &'a str,
    pub queue: &'a Path,
    pub pause: &'a Path,
    pub claims: &'a Claims,
    pub worker: &'a Worker,
    pub partition: &'a Partition,
    pub policy: &'a Policy,
    pub ttl: u64,
}
impl Consumer<'_> {
    fn paused(&self) -> Result<bool> {
        match fs::symlink_metadata(self.pause) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err("cannot establish board pause state".into()),
        }
    }

    /// At most one task; any lease, even expired, blocks the worker until fenced completion.
    /// Unknown counts refuse. Audit durably records the board before reservation, without packets.
    /// Retain the original receipt durably before publication; never reacquire the queue lock.
    /// Any retention/publication error preserves the charged reservation for reconciliation.
    pub fn once(
        &self,
        source: &mut impl Source,
        packets: &[Packet<'_>],
        now: u64,
        mut audit: impl FnMut(&AssignmentBoard) -> Result<()>,
        mut retain: impl FnMut(&Receipt) -> Result<()>,
    ) -> Result<Vec<Receipt>> {
        if !self.policy.enabled || self.paused()? {
            return Ok(vec![]);
        }
        let host = source.host_id()?.map(str::to_owned);
        self.partition.validate(self.policy.max_attempts_per_day)?;
        let board = self.claims.assignment_board(
            &mut Partitioned(source, self.partition),
            self.policy,
            std::slice::from_ref(self.worker),
            now,
        )?;
        audit(&board)?;
        for number in board.candidates(&self.worker.id)? {
            let entry = board.board.entries.iter().find(|e| e.number == number).unwrap();
            if !matches!(
                entry.action,
                Action::FixFindings
                    | Action::FixRequiredCi
                    | Action::ResolveConflict
                    | Action::AddRemoves
                    | Action::BuildIssue
            ) {
                continue;
            }
            let matching: Vec<_> = packets.iter().filter(|p| p.number == number).collect();
            let [packet] = matching.as_slice() else { continue };
            if packet.revision != entry.head || packet.action != entry.action {
                continue;
            }
            let identity = serde_json::to_vec(&(self.repository, number, &entry.head, entry.action))
                .map_err(|_| "cannot encode board queue identity")?;
            let key = format!("{:x}", Sha256::digest(identity));
            let mut receipt = None;
            let published = helper_queue::publish_reserved(
                self.queue,
                &key,
                packet.task,
                entry.release_pinned,
                self.policy,
                || {
                    if self.paused()? {
                        return Ok(false);
                    }
                    let rows = self.claims.assignments()?;
                    // Recheck under the shared queue publication lock, including other refs.
                    if !rows.contains_key(&number)
                        || rows.values().any(|row| {
                            row.state
                                .lease
                                .as_ref()
                                .is_some_and(|l| l.holder == self.worker.id)
                        })
                        || rows[&number].state.lease.is_some()
                        || (host.is_none() && !source.revalidate(entry)?)
                        || self.paused()?
                    {
                        return Ok(false);
                    }
                    let claimed = if let Some(host) = &host {
                        self.claims
                            .claim_idle_item_for(
                                entry,
                                self.policy,
                                self.worker,
                                host,
                                (now, self.ttl),
                                |_, retained| {
                                    let existing = source.existing_work(entry, retained)?;
                                    if self.paused()? {
                                        return Err("refill paused during observation".into());
                                    }
                                    Ok(existing)
                                },
                            )?
                            .map(|receipt| receipt.lease)
                    } else {
                        self.claims
                            .claim_idle_for(entry, self.policy, self.worker, now, self.ttl)?
                    };
                    let Some(lease) = claimed else {
                        return Ok(false);
                    };
                    let original = Receipt {
                        number,
                        key: key.clone(),
                        lease,
                    };
                    retain(&original)?;
                    receipt = Some(original);
                    Ok(true)
                },
            )?;
            if published {
                return Ok(vec![receipt.ok_or("missing native queue receipt")?]);
            }
        }
        Ok(vec![])
    }
}
