use oxa_cli::lead::cards::{command, unavailable, Cards, Unavailable};
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
fn absent_standing_revision_stays_unavailable() {
    let mut observation = snapshot();
    observation.standing_revision = 0;
    let cards = Cards::from_snapshot(observation, 1000, 100).unwrap();
    for card in [cards.status(), cards.asks()] {
        assert!(
            card.lines().next().unwrap().contains("standing unavailable"),
            "{card}"
        );
        assert!(!card.contains("standing 0"));
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
    assert!(asks.contains("Ask A1 · PR #7"));
    assert!(asks.contains("Options / recommendation / meanwhile: unavailable"));
    assert!(asks.contains("decision write disabled"));
}

#[test]
fn commands_fetch_current_typed_facts_and_never_route_mutations() {
    let reads = std::cell::Cell::new(0);
    let read = || {
        reads.set(reads.get() + 1);
        let mut s = snapshot();
        s.as_of_seq = reads.get();
        Ok::<_, ()>(s)
    };
    assert!(command("/status", 1000, 100, read).unwrap().contains("seq 1 ·"));
    assert!(command("/asks", 1000, 100, read).unwrap().contains("seq 2 ·"));
    assert_eq!(reads.get(), 2);
    for unknown in ["/hold 7", "/status extra", "/approve 7", "/decide A1 yes"] {
        assert_eq!(command(unknown, 1000, 100, read), None);
    }
    assert_eq!(reads.get(), 2);
    let missing = command("/status", 1000, 100, || Err::<Snapshot, _>("private diagnostic"));
    assert_eq!(missing.as_deref(), Some(unavailable(Unavailable::Invalid)));
    assert!(!missing.unwrap().contains("private diagnostic"));
    assert_eq!(
        command("/asks", 1101, 100, || Ok::<_, ()>(snapshot())).as_deref(),
        Some(unavailable(Unavailable::Stale))
    );
    let mut invalid = snapshot();
    invalid.goal = Some("injected\nstatus".into());
    assert!(matches!(
        Cards::from_snapshot(invalid, 1000, 100),
        Err(Unavailable::Invalid)
    ));
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

#[test]
fn accepted_directional_marks_are_escaped_in_goal_and_ask_cards() {
    let mut observation = snapshot();
    let text = "before\u{061c}\u{200e}\u{200f}after";
    observation.goal = Some(text.into());
    observation.asks.push(Ask {
        id: "A1".into(),
        subject: Subject::Repository,
        question: text.into(),
    });
    let cards = Cards::decode(&observation.to_json().unwrap(), 1000, 100).unwrap();
    for output in [cards.status(), cards.asks()] {
        assert!(output.contains(r"before\u{61c}\u{200e}\u{200f}after"));
        assert!(!output
            .chars()
            .any(|ch| matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}')));
    }
}
