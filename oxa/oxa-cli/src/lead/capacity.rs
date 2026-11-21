//! Owner-configured worker capacity per host (#2369), shown next to observed use.
//!
//! A pure derivation over facts the board already reads: the user-only
//! `oxa.lead.capacity` map, the runner situation and the board projection. Nothing
//! here starts work, refuses work or reads GitHub. An absent input stays unknown,
//! never zero or healthy, and an invalid config entry is ignored and counted rather
//! than refused, so the owner's file stays usable.

use oxa_proto::work::{IncidentKind, Snapshot, Subject};
use oxa_work::board::{Action, Board};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The owner's configured per-host capacity, parsed from the user-layer value of
/// `oxa.lead.capacity`. Unset (or a non-object) is an empty configuration, not an
/// error; the settings layer already reports a wrong type.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct Capacity {
    hosts: BTreeMap<String, u32>,
    ignored: usize,
}

impl Capacity {
    /// Parse the user-layer value. A host name is a non-empty atom of at most 128
    /// characters with no whitespace or control character; a capacity is a whole
    /// number that fits `u32`. Any entry that does not match is ignored and counted.
    pub fn parse(value: Option<&Value>) -> Self {
        let mut out = Self::default();
        let Some(map) = value.and_then(Value::as_object) else {
            return out;
        };
        for (host, count) in map {
            let valid_host = !host.is_empty()
                && host.len() <= 128
                && host.chars().all(|c| !c.is_whitespace() && !c.is_control());
            match (valid_host, count.as_u64().and_then(|n| u32::try_from(n).ok())) {
                (true, Some(n)) => {
                    out.hosts.insert(host.clone(), n);
                }
                _ => out.ignored += 1,
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    /// Entries the owner wrote that could not be read as a host capacity.
    pub fn ignored(&self) -> usize {
        self.ignored
    }

    /// The configured host capacities as a JSON object, for the status section.
    pub fn to_json(&self) -> Value {
        Value::Object(
            self.hosts
                .iter()
                .map(|(host, count)| (host.clone(), json!(count)))
                .collect(),
        )
    }
}

/// One host row: the owner's stated capacity and the observed running workers.
/// Either side may be unknown: a host the owner did not configure has no stated
/// capacity, and without a situation observation no use is known.
#[derive(Debug, PartialEq, Eq)]
pub struct HostRow {
    pub host: String,
    pub capacity: Option<u32>,
    pub used: Option<u32>,
}

/// The capacity truth the pages render, derived only from inputs already read.
#[derive(Debug, Default)]
pub struct Facts {
    /// Configured and observed hosts, sorted; empty only when neither is present.
    pub hosts: Vec<HostRow>,
    /// Ready queue depth: eligible items the loop would propose first. `None`
    /// without a board observation, never a manufactured zero.
    pub ready: Option<usize>,
    /// Stuck work the situation reports (stalled tasks, idle hosts, blocked
    /// task/host subjects). `None` without a situation observation; an empty list
    /// means the situation reported none.
    pub stuck: Option<Vec<String>>,
}

/// Derive the capacity facts from the configured map and the same board and
/// situation the rest of the page renders. No new read happens here.
pub fn derive(capacity: &Capacity, board: Option<&Board>, situation: Option<&Snapshot>) -> Facts {
    let mut used: BTreeMap<&str, u32> = BTreeMap::new();
    if let Some(snapshot) = situation {
        for running in &snapshot.running {
            *used.entry(running.host.as_str()).or_insert(0) += 1;
        }
    }
    let mut hosts: BTreeSet<&str> = capacity.hosts.keys().map(String::as_str).collect();
    hosts.extend(used.keys().copied());
    let hosts = hosts
        .into_iter()
        .map(|host| HostRow {
            host: host.to_owned(),
            capacity: capacity.hosts.get(host).copied(),
            used: situation.map(|_| used.get(host).copied().unwrap_or(0)),
        })
        .collect();
    let ready = board.map(|board| {
        board
            .entries
            .iter()
            .filter(|entry| entry.eligible && entry.action <= Action::Ready)
            .count()
    });
    let stuck = situation.map(stuck_work);
    Facts { hosts, ready, stuck }
}

/// The stuck work one situation reports, as distinct sorted labels. Reuses the
/// same stall/idle subjects the workers section shows, plus blocked task/host
/// subjects; it never invents a state the situation did not report.
fn stuck_work(snapshot: &Snapshot) -> Vec<String> {
    let mut labels: BTreeSet<String> = BTreeSet::new();
    for incident in &snapshot.incidents {
        match (&incident.kind, &incident.subject) {
            (IncidentKind::WorkerStalled, Subject::Task(task)) => {
                labels.insert(format!("task {task} stalled"));
            }
            (IncidentKind::HostIdle, Subject::Host(host)) => {
                labels.insert(format!("host {host} idle"));
            }
            _ => {}
        }
    }
    for blocked in &snapshot.blocked {
        match &blocked.subject {
            Subject::Task(task) => {
                labels.insert(format!("task {task} blocked ({})", blocked.code));
            }
            Subject::Host(host) => {
                labels.insert(format!("host {host} blocked ({})", blocked.code));
            }
            _ => {}
        }
    }
    labels.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::work::{Blocked, Incident, Running, TaskStep};
    use serde_json::json;

    fn running(task: &str, host: &str) -> Running {
        Running {
            task: task.into(),
            host: host.into(),
            branch: format!("agent/{task}"),
            worktree: format!("~/project/{task}"),
            session_id: None,
            step: TaskStep::Worker,
        }
    }

    fn snapshot(running: Vec<Running>) -> Snapshot {
        Snapshot {
            as_of_seq: 1,
            at_ms: 1_000,
            standing_revision: 1,
            goal: None,
            running,
            local_accepted: Vec::new(),
            published: Vec::new(),
            landed: Vec::new(),
            blocked: Vec::new(),
            asks: Vec::new(),
            incidents: Vec::new(),
            history_truncated: false,
        }
    }

    #[test]
    fn parses_valid_entries_and_ignores_invalid_ones() {
        let value = json!({
            "host-a": 6, "host-b": 8, "zero": 0,
            "bad name": 4, "": 3, "neg": -1, "frac": 1.5, "text": "6",
            "huge": u64::from(u32::MAX) + 1,
        });
        let capacity = Capacity::parse(Some(&value));
        assert_eq!(capacity.hosts.get("host-a"), Some(&6));
        assert_eq!(capacity.hosts.get("host-b"), Some(&8));
        assert_eq!(capacity.hosts.get("zero"), Some(&0));
        // bad name, empty, neg, frac, text, huge: six ignored, never a refusal.
        assert_eq!(capacity.ignored(), 6);
        assert!(!capacity.is_empty());
    }

    #[test]
    fn unset_and_non_object_are_empty_not_errors() {
        assert!(Capacity::parse(None).is_empty());
        assert!(Capacity::parse(Some(&json!("host-a"))).is_empty());
        assert!(Capacity::parse(Some(&json!({}))).is_empty());
        assert_eq!(Capacity::parse(None).ignored(), 0);
    }

    #[test]
    fn host_rows_union_configured_and_observed_with_unknown_sides() {
        let capacity = Capacity::parse(Some(&json!({"host-a": 6, "host-b": 8})));
        let situation = snapshot(vec![
            running("t1", "host-a"),
            running("t2", "host-a"),
            running("t3", "other"),
        ]);
        let facts = derive(&capacity, None, Some(&situation));
        let row = |host: &str| facts.hosts.iter().find(|r| r.host == host).unwrap();
        assert_eq!(row("host-a").capacity, Some(6));
        assert_eq!(row("host-a").used, Some(2));
        // Configured but idle: observed use is a real zero, not unknown.
        assert_eq!(row("host-b").used, Some(0));
        // Observed but unconfigured: capacity stays unknown.
        assert_eq!(row("other").capacity, None);
        assert_eq!(row("other").used, Some(1));
        assert_eq!(facts.hosts.len(), 3);
    }

    #[test]
    fn absent_situation_leaves_use_unknown_not_zero() {
        let capacity = Capacity::parse(Some(&json!({"host-a": 6})));
        let facts = derive(&capacity, None, None);
        assert_eq!(facts.hosts.len(), 1);
        assert_eq!(facts.hosts[0].capacity, Some(6));
        assert_eq!(facts.hosts[0].used, None);
        assert_eq!(facts.ready, None);
        assert_eq!(facts.stuck, None);
    }

    #[test]
    fn ready_depth_counts_only_eligible_proposable_items() {
        use oxa_work::board::Entry;
        let entry = |number: u64, eligible: bool, action: Action| Entry {
            number,
            head: "synthetic-head".into(),
            release_pinned: false,
            action,
            lease: None,
            attempts_today: 0,
            eligible,
            evidence_incomplete: None,
        };
        let board = Board {
            entries: vec![
                entry(1, true, Action::Ready),
                entry(2, true, Action::BuildIssue),
                entry(3, false, Action::Ready),
                entry(4, true, Action::WaitReview),
                entry(5, true, Action::EvidenceIncomplete),
            ],
            ..Board::default()
        };
        let facts = derive(&Capacity::default(), Some(&board), None);
        assert_eq!(facts.ready, Some(2));
    }

    #[test]
    fn stuck_reports_stalled_idle_and_blocked_subjects() {
        let mut situation = snapshot(vec![running("t1", "host-a")]);
        situation.incidents = vec![
            Incident {
                id: "i1".into(),
                subject: Subject::Task("t1".into()),
                kind: IncidentKind::WorkerStalled,
                evidence: None,
            },
            Incident {
                id: "i2".into(),
                subject: Subject::Host("host-a".into()),
                kind: IncidentKind::HostIdle,
                evidence: None,
            },
        ];
        situation.blocked = vec![Blocked {
            subject: Subject::Task("t2".into()),
            code: "ci".into(),
            ask_id: None,
        }];
        let facts = derive(&Capacity::default(), None, Some(&situation));
        assert_eq!(
            facts.stuck,
            Some(vec![
                "host host-a idle".to_owned(),
                "task t1 stalled".to_owned(),
                "task t2 blocked (ci)".to_owned(),
            ])
        );
    }

    #[test]
    fn empty_situation_reports_no_stuck_not_unknown() {
        let facts = derive(&Capacity::default(), None, Some(&snapshot(vec![])));
        assert_eq!(facts.stuck, Some(Vec::new()));
        assert!(facts.hosts.is_empty());
    }
}
