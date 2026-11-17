//! Correlate a verified rejection with an earlier owner-observed queue admission.
use super::{Board, File, QueueObservation, Read, Seek, SeekFrom, MAX_LOG_BYTES};
use crate::board::queue::{is_object_id, Removal};
use crate::Result;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::BufReader};

#[derive(Deserialize)]
struct PriorSample {
    schema_version: u32,
    observed_at: u64,
    board: Board,
    #[serde(default)]
    landing: BTreeMap<u64, PriorLanding>,
    #[serde(default)]
    queue_rejections: Vec<PriorRejection>,
    landing_main: Option<String>,
}

#[derive(Deserialize)]
struct PriorRejection {
    number: u64,
    removal_event: String,
    merge_group_head: String,
    removed_at: Option<String>,
}

#[derive(Deserialize)]
struct PriorLanding {
    queue: Option<QueueObservation>,
}

struct Admission {
    head: String,
    group: String,
    enqueued_at: u64,
    observed_at: u64,
}

#[derive(Clone, Serialize)]
pub(super) struct AdmissionEvidence {
    removal_event: String,
    pr_head: String,
    merge_group_head: String,
    enqueued_at: u64,
    admission_observed_at: u64,
    #[serde(skip)]
    removed_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    rejection_baseline: Option<RejectionBaseline>,
}

/// Actual main observed during the first matching rejection snapshot.
#[derive(Clone, Serialize)]
struct RejectionBaseline {
    main: String,
    observed_at: u64,
}

#[derive(Default)]
pub(super) struct Admissions {
    queued: BTreeMap<u64, Admission>,
    matched: BTreeMap<u64, AdmissionEvidence>,
}

impl Admissions {
    pub(super) fn load(log: &mut File) -> Result<Self> {
        log.seek(SeekFrom::Start(0))
            .map_err(|_| "cannot seek owner shadow journal")?;
        let mut history = Self::default();
        let reader = BufReader::new(log.take(MAX_LOG_BYTES));
        for prior in serde_json::Deserializer::from_reader(reader).into_iter::<PriorSample>() {
            let prior = prior.map_err(|_| "cannot read owner shadow journal")?;
            if prior.schema_version != 1 || prior.observed_at != prior.board.observed_at {
                return Err("invalid owner shadow journal sample".into());
            }
            if prior.landing.is_empty() {
                continue;
            }
            history.matches(
                &prior.board,
                prior.landing_main.as_deref(),
                prior.queue_rejections.iter().filter_map(|r| {
                    Some(Removal {
                        number: r.number,
                        event: &r.removal_event,
                        group: &r.merge_group_head,
                        removed_at: r.removed_at.as_deref()?,
                    })
                }),
            );
            history.observe(
                &prior.board,
                prior.landing.iter().map(|(n, row)| (*n, row.queue.as_ref())),
            );
        }
        Ok(history)
    }

    pub(super) fn observe<'a>(
        &mut self,
        board: &Board,
        landing: impl Iterator<Item = (u64, Option<&'a QueueObservation>)>,
    ) {
        let landing: BTreeMap<_, _> = landing.collect();
        if landing.is_empty() {
            return;
        }
        let active: BTreeMap<_, _> = board
            .entries
            .iter()
            .map(|entry| (entry.number, entry.head.as_str()))
            .collect();
        self.queued
            .retain(|number, admission| active.get(number).copied() == Some(admission.head.as_str()));
        for (number, queued) in landing {
            let Some(head) = active.get(&number).copied().filter(|head| is_object_id(head)) else {
                continue;
            };
            let Some(queue) = queued.filter(|queue| queue.enqueued_at <= board.observed_at) else {
                continue;
            };
            let Some(group) = queue.group_head.as_deref().filter(|group| is_object_id(group)) else {
                continue;
            };
            if self
                .queued
                .get(&number)
                .is_some_and(|prior| prior.group == group && prior.enqueued_at == queue.enqueued_at)
            {
                continue;
            }
            self.queued.insert(
                number,
                Admission {
                    head: head.to_owned(),
                    group: group.to_owned(),
                    enqueued_at: queue.enqueued_at,
                    observed_at: board.observed_at,
                },
            );
        }
    }

    pub(super) fn matches<'a>(
        &mut self,
        board: &Board,
        main: Option<&str>,
        rejections: impl Iterator<Item = Removal<'a>>,
    ) -> BTreeMap<u64, AdmissionEvidence> {
        let active: BTreeMap<_, _> = board
            .entries
            .iter()
            .map(|entry| (entry.number, entry.head.as_str()))
            .collect();
        let main = main.filter(|head| is_object_id(head));
        self.matched.retain(|number, known| {
            active.get(number).copied() == Some(known.pr_head.as_str())
                && self.queued.get(number).is_some_and(|prior| {
                    prior.group == known.merge_group_head
                        && prior.enqueued_at == known.enqueued_at
                        && prior.observed_at == known.admission_observed_at
                })
        });
        rejections
            .filter_map(|removal| {
                let Removal {
                    number,
                    event,
                    group,
                    removed_at,
                } = removal;
                let prior = self.queued.get(&number)?;
                let removed_at = oxa_session::parse_utc_seconds(removed_at)?;
                if active.get(&number).copied() != Some(prior.head.as_str())
                    || prior.group != group
                    || removed_at <= prior.observed_at
                    || removed_at > board.observed_at
                {
                    return None;
                }
                let previous = self.matched.get(&number);
                if previous.is_some_and(|known| {
                    removed_at < known.removed_at
                        || (known.removal_event == event) != (known.removed_at == removed_at)
                }) {
                    return None;
                }
                let baseline = previous
                    .filter(|known| known.removal_event == event)
                    .and_then(|known| known.rejection_baseline.as_ref())
                    .filter(|baseline| baseline.observed_at <= board.observed_at)
                    .cloned()
                    .or_else(|| {
                        main.map(|main| RejectionBaseline {
                            main: main.to_owned(),
                            observed_at: board.observed_at,
                        })
                    });
                let observed = AdmissionEvidence {
                    removal_event: event.to_owned(),
                    pr_head: prior.head.clone(),
                    merge_group_head: prior.group.clone(),
                    enqueued_at: prior.enqueued_at,
                    admission_observed_at: prior.observed_at,
                    removed_at,
                    rejection_baseline: baseline,
                };
                self.matched.insert(number, observed.clone());
                Some((number, observed))
            })
            .collect()
    }
}
