//! A synthetic evening for lead status/cards. JSONL contains versioned Event, Snapshot and
//! EventPage documents; snapshots/pages are read views, not additional journal entries.
//! This tests the public wire types and scenario, not a runner's materialization policy.
use oxa_proto::work::*;
use serde_json::Value;

struct Evening {
    events: Vec<Event>,
    snapshots: Vec<Snapshot>,
    page: EventPage,
}

fn evening() -> Evening {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/work/evening.jsonl");
    let bytes = std::fs::read(path).expect("committed evening fixture");
    let mut lines = bytes.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let torn = lines.pop().unwrap();
    assert!(!torn.is_empty(), "fixture ends with an unterminated event");
    assert!(Event::from_json(torn).is_err());
    assert!(serde_json::from_slice::<Value>(torn).unwrap_err().is_eof());

    let mut events = Vec::new();
    let mut snapshots = Vec::new();
    let mut pages = Vec::new();
    for (index, line) in lines.into_iter().enumerate() {
        let value: Value = serde_json::from_slice(line).unwrap();
        match value["type"].as_str().unwrap() {
            "oxa_work_event" => {
                let event = Event::from_json(line).unwrap();
                assert_eq!(event.seq, events.len() as u64 + 1, "line {}", index + 1);
                assert_eq!(Event::from_json(&event.to_json().unwrap()).unwrap(), event);
                events.push(event);
            }
            "oxa_work_situation" => snapshots.push(Snapshot::from_json(line).unwrap()),
            "oxa_work_events" => pages.push(EventPage::from_json(line).unwrap()),
            kind => panic!("unexpected fixture document {kind}"),
        }
    }
    assert_eq!(events.len(), 22);
    assert_eq!(snapshots.len(), 3);
    assert_eq!(pages.len(), 1);
    Evening {
        events,
        snapshots,
        page: pages.pop().unwrap(),
    }
}

fn pr(number: u64, digit: &str) -> PullRequest {
    PullRequest {
        number,
        head: digit.repeat(40),
    }
}

fn local(task: &str, digit: &str, check: &str) -> LocalAcceptance {
    LocalAcceptance {
        task: task.into(),
        commit: digit.repeat(40),
        evidence: Evidence {
            kind: EvidenceKind::LocalCheck,
            id: check.into(),
        },
    }
}

fn red() -> Incident {
    Incident {
        id: "red-queue".into(),
        subject: Subject::PullRequest(102),
        kind: IncidentKind::QueueRejected,
        evidence: Some(Evidence {
            kind: EvidenceKind::Gate,
            id: "gate-red".into(),
        }),
    }
}

fn idle() -> Incident {
    Incident {
        id: "helper-idle".into(),
        subject: Subject::Host("helper".into()),
        kind: IncidentKind::HostIdle,
        evidence: Some(Evidence {
            kind: EvidenceKind::Status,
            id: "idle-since-1900".into(),
        }),
    }
}

fn ask() -> Ask {
    Ask {
        id: "ask-blocked".into(),
        subject: Subject::Task("task-blocked".into()),
        question: "Which synthetic interface should the task target?".into(),
    }
}

#[test]
fn work_fixtures_snapshots_at_three_cursors() {
    let evening = evening();
    let landed = Snapshot {
        as_of_seq: 7,
        at_ms: 1794848700000,
        standing_revision: 1,
        goal: Some("Finish the synthetic evening".into()),
        running: vec![],
        local_accepted: vec![local("task-landed", "a", "local-landed")],
        published: vec![Publication {
            task: "task-landed".into(),
            pr: pr(101, "a"),
        }],
        landed: vec![Landed {
            pr: pr(101, "a"),
            merge_commit: "b".repeat(40),
            merged_at_ms: 1794848700000,
        }],
        blocked: vec![],
        asks: vec![],
        incidents: vec![],
        history_truncated: false,
    };
    let mut rejected = landed.clone();
    rejected.as_of_seq = 15;
    rejected.at_ms = 1794850800000;
    rejected.local_accepted.push(local("task-red", "c", "local-red"));
    rejected.published.push(Publication {
        task: "task-red".into(),
        pr: pr(102, "c"),
    });
    rejected.incidents.push(red());
    let mut blocked = rejected.clone();
    blocked.as_of_seq = 22;
    blocked.at_ms = 1794862800000;
    blocked.blocked = vec![
        Blocked {
            subject: Subject::Task("task-blocked".into()),
            code: "needs-interface".into(),
            ask_id: Some("ask-blocked".into()),
        },
        Blocked {
            subject: Subject::PullRequest(102),
            code: "held".into(),
            ask_id: None,
        },
    ];
    blocked.asks.push(ask());
    blocked.incidents.push(idle());
    assert_eq!(evening.snapshots, vec![landed, rejected, blocked]);
    for snapshot in evening.snapshots {
        assert_eq!(
            Snapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
            snapshot
        );
        let cursor_event = &evening.events[snapshot.as_of_seq as usize - 1];
        assert_eq!(snapshot.at_ms, cursor_event.at_ms);
    }
}

#[test]
fn work_fixtures_evening_preserves_acceptance_failure_hold_and_idle() {
    let evening = evening();
    let facts = evening.events.iter().map(|event| &event.fact).collect::<Vec<_>>();
    let tasks = facts
        .iter()
        .filter_map(|fact| match fact {
            Fact::TaskStarted(running) => Some(running.task.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tasks, ["task-landed", "task-red", "task-blocked"]);
    assert!(matches!(facts[6], Fact::PrMerged(landed) if landed.pr == pr(101, "a")));
    assert!(matches!(facts[9], Fact::LocalAcceptance(local) if local.task == "task-red"));
    assert!(
        matches!(facts[11], Fact::ExternalAcceptance(accepted) if accepted.pr == pr(102, "c") && accepted.evidence.kind == EvidenceKind::Gate)
    );
    // A successful merge-group check identifies its integration commit as evidence only.
    // It does not say that PR 102 landed, even before the subsequent red gate.
    assert!(
        matches!(facts[13], Fact::ExternalAcceptance(accepted) if accepted.pr == pr(102, "c") && accepted.evidence.kind == EvidenceKind::Queue && accepted.evidence.id == format!("merge-group/{}", "d".repeat(40)))
    );
    assert_eq!(facts[14], &Fact::Incident(red()));
    assert!(matches!(facts[16], Fact::WorkerFinished(worker) if worker.outcome == WorkerOutcome::Blocked));
    assert_eq!(facts[17], &Fact::AskOpened(ask()));
    assert!(matches!(facts[18], Fact::Blocked(blocked) if blocked.ask_id.as_deref() == Some("ask-blocked")));
    assert!(
        matches!(facts[19], Fact::Blocked(blocked) if blocked.subject == Subject::PullRequest(102) && blocked.code == "held")
    );
    assert_eq!(facts[20], &Fact::Incident(idle()));
    assert!(matches!(facts[21], Fact::Mechanical(mechanical) if mechanical.kind == MechanicalKind::Refill));
    assert_eq!(
        evening.events[21].at_ms - evening.events[20].at_ms,
        2 * 60 * 60 * 1000
    );
    for snapshot in evening.snapshots {
        assert_eq!(snapshot.landed.len(), 1);
        assert_eq!(snapshot.landed[0].pr.number, 101);
        assert!(snapshot.running.is_empty());
    }
}

#[test]
fn work_fixtures_gap_page_matches_retained_evening_events() {
    let evening = evening();
    assert_eq!(
        evening.page,
        EventPage {
            after: 10,
            first_available: 14,
            gap: true,
            next: 22,
            events: evening.events[13..].to_vec(),
        }
    );
    assert_eq!(
        EventPage::from_json(&evening.page.to_json().unwrap()).unwrap(),
        evening.page
    );
    let mut hidden_gap = evening.page;
    hidden_gap.gap = false;
    assert!(hidden_gap.to_json().is_err());
}
