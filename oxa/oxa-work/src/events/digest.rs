//! A bounded, deterministic digest of facts observed since an owner's cursor.
use super::{Facts, Result};
use oxa_proto::work::{Blocked, Fact, Incident, IncidentKind, Landed, Snapshot, Subject};

#[derive(Debug, PartialEq, Eq)]
pub enum Digest {
    Quiet { through: u64 },
    Changes(DigestChanges),
    SnapshotOnly { snapshot: Snapshot, reason: DigestGap },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DigestGap {
    Retention,
    PageLimit,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DigestChanges {
    pub after: u64,
    pub through: u64,
    pub merged: Vec<Landed>,
    pub blocked: Vec<Blocked>,
    pub cleared: Vec<Subject>,
    /// These events establish an observed stall, not that a worker is still stalled.
    pub observed_stalls: Vec<Incident>,
    pub asks_waiting: usize,
}

impl Facts {
    /// Read one coherent bounded window. Callers can show the fresh snapshot when
    /// the complete interval is unavailable, without representing it as a delta.
    pub fn digest_since(&self, after: u64) -> Result<Digest> {
        let (snapshot, page) = self.read_window(after, 64)?;
        if page.gap || page.next != snapshot.as_of_seq {
            return Ok(Digest::SnapshotOnly {
                reason: if page.gap {
                    DigestGap::Retention
                } else {
                    DigestGap::PageLimit
                },
                snapshot,
            });
        }
        let mut changes = DigestChanges {
            after,
            through: page.next,
            merged: Vec::new(),
            blocked: Vec::new(),
            cleared: Vec::new(),
            observed_stalls: Vec::new(),
            asks_waiting: snapshot.asks.len(),
        };
        let mut asks_changed = false;
        for event in page.events {
            match event.fact {
                Fact::PrMerged(landed) => {
                    changes.merged.retain(|prior| prior.pr.number != landed.pr.number);
                    changes.merged.push(landed);
                }
                Fact::Blocked(block) => {
                    changes.cleared.retain(|subject| *subject != block.subject);
                    changes.blocked.retain(|prior| prior.subject != block.subject);
                    changes.blocked.push(block);
                }
                Fact::Unblocked(subject) => {
                    changes.blocked.retain(|prior| prior.subject != subject);
                    changes.cleared.retain(|prior| *prior != subject);
                    changes.cleared.push(subject);
                }
                Fact::AskOpened(_) | Fact::AskClosed(_) => asks_changed = true,
                Fact::StandingChanged(standing) => {
                    asks_changed |= standing.ask_opened.is_some() || standing.ask_closed.is_some();
                }
                Fact::Incident(incident) if incident.kind == IncidentKind::WorkerStalled => {
                    changes
                        .observed_stalls
                        .retain(|prior| prior.subject != incident.subject);
                    changes.observed_stalls.push(incident);
                }
                _ => {}
            }
        }
        if changes.merged.is_empty()
            && changes.blocked.is_empty()
            && changes.cleared.is_empty()
            && changes.observed_stalls.is_empty()
            && !asks_changed
        {
            return Ok(Digest::Quiet { through: page.next });
        }
        Ok(Digest::Changes(changes))
    }
}
