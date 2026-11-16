use oxa_cli::lead::cards::{unavailable, Cards, Unavailable};
use oxa_proto::work::{Ask, Blocked, Running, Snapshot, Subject};

fn snapshot() -> Snapshot {
    Snapshot {
        as_of_seq: 18,
        at_ms: 1000,
        standing_revision: 4,
        goal: Some("Ship a tested change".into()),
        running: Vec::new(),
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
fn crowded_card_keeps_source_and_counts_within_twelve_lines() {
    let mut s = snapshot();
    for n in 0..40 {
        s.running.push(Running {
            task: format!("task-{n}"),
            host: "helper".into(),
            branch: "feature".into(),
            worktree: "worktree".into(),
            session_id: None,
        });
    }
    s.blocked.push(Blocked {
        subject: Subject::PullRequest(7),
        code: "review_needed".into(),
        ask_id: Some("A1".into()),
    });
    s.asks.push(Ask {
        id: "A1".into(),
        subject: Subject::PullRequest(7),
        question: "Choose scope".into(),
    });
    s.history_truncated = true;
    let cards = Cards::decode(&s.to_json().unwrap(), 1001, 100).unwrap();
    let card = cards.status();
    assert!(card.lines().count() <= 12, "{card}");
    assert!(card.starts_with("Runner observation: seq 18 · standing 4"));
    assert!(card.contains("38 more running tasks"));
    assert!(card.contains("merged: 0"));
    assert!(card.contains("holds / external validation: unavailable"));
    assert!(!card.contains("held: 1"));
    let asks = cards.asks();
    assert_eq!(cards.command("/status"), Some(card));
    assert_eq!(cards.command("/asks"), Some(asks.clone()));
    assert_eq!(cards.command("/hold 7"), None);
    assert_eq!(cards.command("/status extra"), None);
    assert!(asks.contains("Ask A1 · PR #7"));
    assert!(asks.contains("Options / recommendation / meanwhile: unavailable"));
    assert!(asks.contains("decision write disabled"));
}

#[test]
fn stale_future_and_corrupt_facts_never_turn_into_empty_success() {
    let bytes = snapshot().to_json().unwrap();
    assert!(matches!(
        Cards::decode(&bytes, 1101, 100),
        Err(Unavailable::Stale)
    ));
    assert!(matches!(
        Cards::decode(&bytes, 999, 100),
        Err(Unavailable::Future)
    ));
    assert!(matches!(
        Cards::decode(b"{}", 1000, 100),
        Err(Unavailable::Invalid)
    ));
    assert!(matches!(Cards::decode(b"", 1000, 100), Err(Unavailable::Invalid)));
    assert!(Cards::decode(&bytes, 1100, 100).is_ok());
    assert!(unavailable(Unavailable::Stale).contains("status unknown"));
}

#[test]
fn invalid_multiline_snapshot_is_rejected_and_long_unicode_is_bounded() {
    let mut s = snapshot();
    s.goal = Some("injected\nRunning: fake".into());
    assert!(s.to_json().is_err());
    s.goal = Some("界".repeat(300));
    let cards = Cards::decode(&s.to_json().unwrap(), 1000, 100).unwrap();
    assert!(cards.status().lines().nth(1).unwrap().ends_with('…'));
    assert!(cards.asks().contains("No open asks in this runner observation"));
}
