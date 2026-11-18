//! Compose the lead cards with the runner's public facts store.
use oxa_cli::lead::cards::command;
use oxa_fake::TestEnv;
use oxa_proto::work::{
    Ask, AskClosed, Blocked, Checkpoint, Evidence, EvidenceKind, Fact, Landed, LocalAcceptance, Publication,
    PullRequest, Running, Subject, TaskStep,
};
use oxa_work::events::Facts;

#[test]
fn missing_public_store_stays_unknown_without_initialization() {
    let env = TestEnv::new();
    let root = env.root().join("absent-facts");
    for name in ["/status", "/asks"] {
        let card = command(name, 1000, 100, || Facts::open(&root)?.situation()).unwrap();
        assert!(card.contains("status unknown"));
        assert!(!root.exists());
    }
}

#[test]
fn commands_read_fresh_public_storage_and_keep_acceptance_distinct_from_landing() {
    let env = TestEnv::new();
    let facts = Facts::initialize(&env.root().join("facts")).unwrap();
    let head = "1".repeat(40);
    let observations = [
        Fact::TaskStarted(Running {
            task: "task-1".into(),
            host: "helper".into(),
            branch: "agent/task-1".into(),
            worktree: "~/project/task-1".into(),
            session_id: None,
            step: TaskStep::Preparing,
        }),
        Fact::LocalAcceptance(LocalAcceptance {
            task: "task-1".into(),
            commit: head.clone(),
            evidence: Evidence {
                kind: EvidenceKind::LocalCheck,
                id: "checks-1".into(),
            },
        }),
        Fact::Blocked(Blocked {
            subject: Subject::PullRequest(7),
            code: "needs_review".into(),
            ask_id: Some("ask-1".into()),
        }),
        Fact::AskOpened(Ask {
            id: "ask-1".into(),
            subject: Subject::PullRequest(7),
            question: "Choose scope".into(),
        }),
    ];
    for (index, fact) in observations.into_iter().enumerate() {
        facts
            .append(
                Checkpoint {
                    source: "fixture".into(),
                    revision: index as u64 + 1,
                },
                1000,
                fact,
            )
            .unwrap();
    }
    let status = command("/status", 1000, 100, || facts.situation()).unwrap();
    assert!(status.contains("seq 4 ·"));
    assert!(status.contains("standing unavailable"));
    assert!(status.contains("Running: 1 · locally accepted: 1 · published: 0 · merged: 0"));
    assert!(status.contains("Queue / holds / external validation: unavailable"));
    assert!(status.contains("Block: PR #7 · needs_review"));
    assert!(status.lines().count() <= 12);
    let asks = command("/asks", 1000, 100, || facts.situation()).unwrap();
    assert!(asks.contains("Ask ask-1 · PR #7"));
    facts
        .append(
            Checkpoint {
                source: "fixture".into(),
                revision: 5,
            },
            1001,
            Fact::AskClosed(AskClosed { id: "ask-1".into() }),
        )
        .unwrap();
    let next = command("/asks", 1001, 100, || facts.situation()).unwrap();
    assert!(next.contains("seq 5 ·"));
    assert!(next.contains("No open asks"));
    let pr = PullRequest {
        number: 7,
        head: head.clone(),
    };
    facts
        .append(
            Checkpoint {
                source: "fixture".into(),
                revision: 6,
            },
            1002,
            Fact::Published(Publication {
                task: "task-1".into(),
                pr: pr.clone(),
            }),
        )
        .unwrap();
    let published = command("/status", 1002, 100, || facts.situation()).unwrap();
    assert!(published.contains("seq 6 ·"));
    assert!(published.contains("locally accepted: 1 · published: 1 · merged: 0"));
    facts
        .append(
            Checkpoint {
                source: "fixture".into(),
                revision: 7,
            },
            1003,
            Fact::PrMerged(Landed {
                pr,
                merge_commit: head,
                merged_at_ms: 1003,
            }),
        )
        .unwrap();
    let landed = command("/status", 1003, 100, || facts.situation()).unwrap();
    assert!(landed.contains("seq 7 ·"));
    assert!(landed.contains("locally accepted: 0 · published: 0 · merged: 1"));
}
