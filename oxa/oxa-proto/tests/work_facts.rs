use oxa_proto::work::*;
use serde_json::{json, Value};

fn pr() -> PullRequest {
    PullRequest {
        number: 123,
        head: "a".repeat(40),
    }
}
fn evidence(kind: EvidenceKind) -> Evidence {
    Evidence {
        kind,
        id: "check-17".into(),
    }
}
fn running() -> Running {
    Running {
        task: "task-17".into(),
        host: "helper".into(),
        branch: "agent/17-work".into(),
        worktree: "~/project/task-17".into(),
        session_id: Some("session-17".into()),
    }
}
fn ask() -> Ask {
    Ask {
        id: "ask-17".into(),
        subject: Subject::PullRequest(123),
        question: "Release the held subject?".into(),
    }
}
fn event(seq: u64, fact: Fact) -> Event {
    Event {
        seq,
        at_ms: 1700000000000,
        checkpoint: Checkpoint {
            source: "runner".into(),
            revision: seq,
        },
        fact,
    }
}
fn incident(kind: IncidentKind) -> Incident {
    Incident {
        id: "incident-17".into(),
        subject: Subject::Task("task-17".into()),
        kind,
        evidence: Some(evidence(EvidenceKind::Status)),
    }
}
fn snapshot() -> Snapshot {
    Snapshot {
        as_of_seq: 12,
        at_ms: 1700000000000,
        standing_revision: 3,
        goal: Some("Finish the controller".into()),
        running: vec![running()],
        local_accepted: vec![LocalAcceptance {
            task: "task-17".into(),
            commit: "a".repeat(40),
            evidence: evidence(EvidenceKind::LocalCheck),
        }],
        published: vec![Publication {
            task: "task-17".into(),
            pr: pr(),
        }],
        landed: vec![Landed {
            pr: pr(),
            merge_commit: "b".repeat(40),
            merged_at_ms: 1700000000000,
        }],
        blocked: vec![Blocked {
            subject: Subject::PullRequest(123),
            code: "held".into(),
            ask_id: Some("ask-17".into()),
        }],
        asks: vec![ask()],
        incidents: vec![incident(IncidentKind::WorkerStalled)],
        history_truncated: true,
    }
}

#[test]
fn independent_lead_fixture_distinguishes_each_completion_stage() {
    let stages = [
        Fact::WorkerFinished(WorkerFinished {
            task: "task-17".into(),
            session_id: Some("session-17".into()),
            outcome: WorkerOutcome::Done,
            error_class: None,
        }),
        Fact::LocalAcceptance(LocalAcceptance {
            task: "task-17".into(),
            commit: "a".repeat(40),
            evidence: evidence(EvidenceKind::LocalCheck),
        }),
        Fact::Published(Publication {
            task: "task-17".into(),
            pr: pr(),
        }),
        Fact::ExternalAcceptance(ExternalAcceptance {
            pr: pr(),
            evidence: evidence(EvidenceKind::Gate),
        }),
        Fact::PrMerged(Landed {
            pr: pr(),
            merge_commit: "b".repeat(40),
            merged_at_ms: 1700000000000,
        }),
    ];
    for (index, stage) in stages.into_iter().enumerate() {
        let event = event(index as u64 + 1, stage);
        let encoded = event.to_json().unwrap();
        let json: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(json["data"]["fact"]["kind"] == "pr_merged", index == 4);
        assert_eq!(Event::from_json(&encoded).unwrap(), event);
    }
    let situation = snapshot();
    assert_eq!(
        Snapshot::from_json(&situation.to_json().unwrap()).unwrap(),
        situation
    );
    assert_eq!(situation.standing_revision, 3);
    assert!(situation.history_truncated);
}

#[test]
fn classification_is_fixed_for_routine_mechanical_and_attention_observations() {
    for fact in [
        Fact::TaskStarted(running()),
        Fact::Enqueued(pr()),
        Fact::AskClosed(AskClosed { id: "ask-17".into() }),
    ] {
        assert_eq!(fact.class(), EventClass::Routine);
    }
    for kind in [
        MechanicalKind::TransientRetry,
        MechanicalKind::NoBuildRequeue,
        MechanicalKind::Refill,
        MechanicalKind::CleanBranchUpdate,
    ] {
        let fact = Fact::Mechanical(Mechanical {
            subject: Subject::Repository,
            kind,
        });
        assert_eq!(fact.class(), EventClass::Mechanical);
        assert_eq!(
            Event::from_json(&event(1, fact.clone()).to_json().unwrap())
                .unwrap()
                .fact,
            fact
        );
    }
    for kind in [
        IncidentKind::TransientRefusal,
        IncidentKind::NoBuildRequeue,
        IncidentKind::RuleReminder,
    ] {
        assert_eq!(Fact::Incident(incident(kind)).class(), EventClass::Mechanical);
    }
    for kind in [
        IncidentKind::RuleConflict,
        IncidentKind::ReviewLimit,
        IncidentKind::ReservedAction,
        IncidentKind::BriefMissing,
        IncidentKind::HostIdle,
        IncidentKind::BudgetWarning,
        IncidentKind::BudgetExhausted,
        IncidentKind::WorkerStalled,
        IncidentKind::QueueRejected,
        IncidentKind::MainFailed,
        IncidentKind::SevereFinding,
        IncidentKind::InboxMessage,
        IncidentKind::UnreadInbox,
    ] {
        let fact = Fact::Incident(incident(kind));
        assert_eq!(fact.class(), EventClass::Attention);
        assert_eq!(
            Event::from_json(&event(1, fact.clone()).to_json().unwrap())
                .unwrap()
                .fact,
            fact
        );
    }
    for fact in [
        Fact::AskOpened(ask()),
        Fact::Blocked(snapshot().blocked.remove(0)),
    ] {
        assert_eq!(fact.class(), EventClass::Attention);
    }
}

#[test]
fn cursor_pages_show_a_retention_gap_and_never_silently_skip_an_event() {
    let page = EventPage {
        after: 2,
        first_available: 5,
        gap: true,
        next: 6,
        events: vec![event(5, Fact::Enqueued(pr())), event(6, Fact::Enqueued(pr()))],
    };
    assert_eq!(EventPage::from_json(&page.to_json().unwrap()).unwrap(), page);
    let mut broken = page.clone();
    broken.gap = false;
    assert!(broken.to_json().is_err());
    let mut broken = page.clone();
    broken.events[1].seq = 7;
    assert!(broken.to_json().is_err());
    let mut broken = page.clone();
    broken.next = 7;
    assert!(broken.to_json().is_err());
    let empty = EventPage {
        after: 2,
        first_available: 5,
        gap: true,
        next: 4,
        events: vec![],
    };
    assert_eq!(EventPage::from_json(&empty.to_json().unwrap()).unwrap(), empty);
    let current = EventPage {
        after: 6,
        first_available: 5,
        gap: false,
        next: 6,
        events: vec![],
    };
    assert_eq!(
        EventPage::from_json(&current.to_json().unwrap()).unwrap(),
        current
    );
}

#[test]
fn an_exhausted_sequence_cannot_return_the_cursor_itself_again() {
    let duplicate = EventPage {
        after: u64::MAX,
        first_available: u64::MAX,
        gap: false,
        next: u64::MAX,
        events: vec![event(u64::MAX, Fact::Enqueued(pr()))],
    };
    assert!(duplicate.to_json().is_err());
    let terminal = EventPage {
        events: vec![],
        ..duplicate
    };
    assert_eq!(
        EventPage::from_json(&terminal.to_json().unwrap()).unwrap(),
        terminal
    );
}

#[test]
fn malformed_documents_cannot_claim_a_current_healthy_situation() {
    let original: Value = serde_json::from_slice(&snapshot().to_json().unwrap()).unwrap();
    for (pointer, bad) in [
        ("/version", json!(2)),
        ("/type", json!("something_else")),
        ("/data/as_of_seq", json!(-1)),
        ("/data/landed/0/pr/head", json!("abcdef0")),
        ("/data/landed/0/merge_commit", json!("Z".repeat(40))),
        ("/data/blocked/0/subject/data", json!(0)),
        ("/data/running/0/session_id", json!("two\nlines")),
        ("/data/asks/0/question", json!("x".repeat(513))),
        ("/data/asks/0/subject/kind", json!("guess")),
    ] {
        let mut bad_doc = original.clone();
        *bad_doc.pointer_mut(pointer).unwrap() = bad;
        assert!(
            Snapshot::from_json(&serde_json::to_vec(&bad_doc).unwrap()).is_err(),
            "{pointer}"
        );
    }
    for pointer in ["", "/data", "/data/running/0"] {
        let mut bad_doc = original.clone();
        bad_doc
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("diagnostic".into(), json!("PRIVATE_MARKER"));
        let err = Snapshot::from_json(&serde_json::to_vec(&bad_doc).unwrap()).unwrap_err();
        assert!(!err.to_string().contains("PRIVATE_MARKER"));
    }
    let mut missing = original.clone();
    missing["data"]
        .as_object_mut()
        .unwrap()
        .remove("standing_revision");
    assert!(Snapshot::from_json(&serde_json::to_vec(&missing).unwrap()).is_err());
    assert!(Snapshot::from_json(b"{}").is_err());
    assert!(Snapshot::from_json(b"").is_err());
    assert!(Snapshot::from_json(b"{\"type\":").is_err());
}

#[test]
fn capacities_are_errors_instead_of_silent_truncation_or_unbounded_input() {
    let mut situation = snapshot();
    situation.running = vec![running(); MAX_ITEMS + 1];
    assert!(situation.to_json().is_err());
    situation.running = vec![running(); MAX_ITEMS];
    // Each list is in bounds, but their combined size is not.
    assert!(situation.to_json().is_err());
    assert!(Event::from_json(&vec![b' '; MAX_EVENT_BYTES + 1]).is_err());
    assert!(Snapshot::from_json(&vec![b' '; MAX_SNAPSHOT_BYTES + 1]).is_err());
    assert!(EventPage::from_json(&vec![b' '; MAX_PAGE_BYTES + 1]).is_err());
    let mut too_big = snapshot();
    too_big.asks = (0..MAX_ITEMS - 6)
        .map(|i| Ask {
            id: format!("ask-{i}"),
            question: "α".repeat(256),
            ..ask()
        })
        .collect();
    // JSON size is bounded separately from row count (including escaped text).
    assert!(too_big.to_json().is_err());
}

#[test]
fn outcomes_and_evidence_are_explicit_and_head_bound() {
    for outcome in [
        WorkerOutcome::Blocked,
        WorkerOutcome::Failed,
        WorkerOutcome::Interrupted,
    ] {
        let mut finished = WorkerFinished {
            task: "task-17".into(),
            session_id: None,
            outcome,
            error_class: None,
        };
        assert!(event(1, Fact::WorkerFinished(finished.clone()))
            .to_json()
            .is_err());
        finished.error_class = Some("stopped".into());
        let original = event(1, Fact::WorkerFinished(finished));
        assert_eq!(Event::from_json(&original.to_json().unwrap()).unwrap(), original);
    }
    let local = LocalAcceptance {
        task: "task-17".into(),
        commit: "a".repeat(40),
        evidence: evidence(EvidenceKind::Gate),
    };
    assert!(event(1, Fact::LocalAcceptance(local)).to_json().is_err());
    let external = ExternalAcceptance {
        pr: pr(),
        evidence: evidence(EvidenceKind::LocalCheck),
    };
    assert!(event(1, Fact::ExternalAcceptance(external)).to_json().is_err());
    let mut absent_revision = event(1, Fact::Enqueued(pr()));
    absent_revision.checkpoint.revision = 0;
    assert!(absent_revision.to_json().is_err());
    let mut zero_seq = absent_revision;
    zero_seq.checkpoint.revision = 1;
    zero_seq.seq = 0;
    assert!(zero_seq.to_json().is_err());
}
