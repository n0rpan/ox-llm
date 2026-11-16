use super::*;
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let root = std::env::temp_dir().join(format!(
                "oxa-standing-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::DirBuilder::new().mode(0o700).create(&root) {
                Ok(()) => return Self(root),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create private fixture: {error}"),
            }
        }
    }
    fn facts(&self) -> Facts {
        Facts::initialize(&self.0.join("facts")).unwrap()
    }
    fn standing(&self, facts: Facts) -> Standing {
        Standing::initialize(&self.0.join("standing"), Authority::Owner, facts).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn hold(active: bool, supersedes: Option<&str>) -> Rule {
    Rule {
        kind: Kind::Hold,
        scope: Scope::PullRequest(480),
        value: Value::Constraint { active },
        said: "Hold this PR until released".into(),
        source: super::super::Source::Command {
            command: "hold 480".into(),
        },
        supersedes: supersedes.map(str::to_owned),
    }
}
fn subject() -> Subject<'static> {
    Subject {
        pr: Some(480),
        task: Some("task"),
        branch: Some("agent/task"),
        host: Some("helper"),
    }
}

#[test]
fn a_fresh_hold_blocks_each_effect_in_a_handle_opened_before_it() {
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts.clone());
    let old_handle = Standing::open(&fixture.0.join("standing"), facts.clone()).unwrap();
    let effects = AtomicUsize::new(0);
    old_handle
        .with_guard(Action::Update, subject(), |_| {
            effects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap()
        .result
        .unwrap();
    let entry = standing.rule(0, Authority::Owner, 10, hold(true, None)).unwrap();
    assert_eq!(entry.state, EntryState::Stored);
    for action in [
        Action::Admit,
        Action::Push,
        Action::Approve,
        Action::Enqueue,
        Action::Update,
    ] {
        let result = old_handle.with_guard(action, subject(), |_| {
            effects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(matches!(
            result,
            Err(GuardError::Refused(Refusal {
                reason: super::super::RefusalReason::Held,
                revision: 1,
                ..
            }))
        ));
    }
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(old_handle.is_held(480).unwrap(), Some(entry.id));
    assert_eq!(facts.situation().unwrap().standing_revision, 1);
}

#[test]
fn supersession_cannot_launder_owner_hold_authority() {
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts);
    let first = standing.rule(0, Authority::Owner, 1, hold(true, None)).unwrap();
    assert!(standing.rule(1, Authority::Lead, 2, hold(false, None)).is_err());
    let second = standing
        .rule(1, Authority::Lead, 2, hold(true, Some(&first.id)))
        .unwrap();
    assert!(standing
        .rule(2, Authority::Lead, 3, hold(false, Some(&second.id)))
        .is_err());
    let mut retired = hold(false, Some(&second.id));
    retired.value = Value::Retired;
    assert!(standing.rule(2, Authority::Lead, 3, retired).is_err());
    assert_eq!(standing.read().unwrap().revision, 2);
    standing
        .rule(2, Authority::Owner, 3, hold(false, Some(&second.id)))
        .unwrap();
    assert!(standing.is_held(480).unwrap().is_none());
    assert!(standing.rule(2, Authority::Owner, 4, hold(true, None)).is_err());
    standing
        .with_guard(Action::Approve, subject(), |read| Ok(read.revision))
        .unwrap()
        .result
        .unwrap();
}

#[test]
fn missing_corrupt_or_symlinked_policy_never_authorizes_the_callback() {
    for poison in ["missing", "corrupt", "symlink"] {
        let fixture = Fixture::new();
        let facts = fixture.facts();
        let standing = fixture.standing(facts.clone());
        let path = fixture.0.join("standing/standing.json");
        fs::remove_file(&path).unwrap();
        let target = fixture.0.join("outside");
        fs::write(&target, b"keep outside").unwrap();
        match poison {
            "missing" => {}
            "corrupt" => fs::write(&path, b"PRIVATE_REJECTED_CONTENT").unwrap(),
            "symlink" => std::os::unix::fs::symlink(&target, &path).unwrap(),
            _ => unreachable!(),
        }
        let effects = AtomicUsize::new(0);
        let result = standing.with_guard(Action::Update, subject(), |_| {
            effects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        assert!(matches!(result, Err(GuardError::Storage(_))));
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert_eq!(fs::read(&target).unwrap(), b"keep outside");
        assert!(Standing::open(&fixture.0.join("standing"), facts).is_err());
    }
}

#[test]
fn pending_revisions_recover_before_or_after_the_fact_without_duplicate_delivery() {
    for appended in [false, true] {
        let fixture = Fixture::new();
        let facts = fixture.facts();
        let standing = fixture.standing(facts.clone());
        let before = standing.read().unwrap();
        let mut after = before.next(0, 9).unwrap();
        let request = hold(true, None);
        after.entries.push(Entry {
            id: "R-1".into(),
            kind: request.kind,
            scope: request.scope,
            value: request.value,
            said: request.said,
            at: 9,
            source: request.source,
            supersedes: None,
            state: EntryState::Stored,
            reminders: 0,
            authority: Authority::Owner,
        });
        let event = Event {
            seq: 1,
            at_ms: 9,
            checkpoint: Checkpoint {
                source: "standing".into(),
                revision: 1,
            },
            fact: Fact::StandingChanged(after.change(None)),
        };
        let observation = serde_json::from_slice(&event.to_json().unwrap()).unwrap();
        if appended {
            facts.append(event.checkpoint, event.at_ms, event.fact).unwrap();
        }
        let document = Document {
            version: 1,
            current: before,
            pending: Some(Pending { after, observation }),
        };
        standing.root.atomic(FILE, &document.bytes().unwrap()).unwrap();
        let reopened = Standing::open(&fixture.0.join("standing"), facts.clone()).unwrap();
        assert_eq!(reopened.read().unwrap().revision, 1);
        assert_eq!(reopened.is_held(480).unwrap().as_deref(), Some("R-1"));
        assert_eq!(facts.events_after(0, 64).unwrap().events.len(), 1);
        assert_eq!(facts.situation().unwrap().standing_revision, 1);
    }
}

#[test]
fn applied_requires_matching_runtime_value_scope_and_revision() {
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts);
    let request = Rule {
        kind: Kind::Goal,
        scope: Scope::Repository,
        value: Value::Text("Finish Rust".into()),
        said: "Finish Rust".into(),
        source: super::super::Source::Command {
            command: "goal Finish Rust".into(),
        },
        supersedes: None,
    };
    let entry = standing.rule(0, Authority::Owner, 1, request).unwrap();
    let observed = fixture.0.join("runtime.json");
    fs::write(&observed, br#"{"goal":"different"}"#).unwrap();
    let checker = |entry: &Entry| -> Result<Verification> {
        let actual: serde_json::Value = serde_json::from_slice(&fs::read(&observed).unwrap()).unwrap();
        Ok(Verification {
            revision: 1,
            scope: entry.scope.clone(),
            value: Value::Text(actual["goal"].as_str().unwrap().into()),
            at: 2,
            evidence: "synthetic-runtime:1".into(),
        })
    };
    assert!(standing.verify(1, &entry.id, checker).is_err());
    assert_eq!(standing.read().unwrap().entries[0].state, EntryState::Stored);
    fs::write(&observed, br#"{"goal":"Finish Rust"}"#).unwrap();
    let applied = standing.verify(1, &entry.id, checker).unwrap();
    assert!(matches!(
        applied.state,
        EntryState::Applied { verified_at: 2, .. }
    ));
    assert_eq!(standing.read().unwrap().revision, 2);
}

#[test]
fn reminders_create_incidents_and_offer_without_changing_advisory_rules() {
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts.clone());
    let request = Rule {
        kind: Kind::Note,
        scope: Scope::Repository,
        value: Value::Text("Keep it brief".into()),
        said: "Keep it brief".into(),
        source: super::super::Source::Command {
            command: "note Keep it brief".into(),
        },
        supersedes: None,
    };
    let note = standing.rule(0, Authority::Owner, 1, request).unwrap();
    assert!(!standing.remind(1, &note.id, 2).unwrap().needs_conversion_offer());
    assert!(standing.remind(2, &note.id, 3).unwrap().needs_conversion_offer());
    let read = standing.read().unwrap();
    assert_eq!(read.entries[0].kind, Kind::Note);
    assert_eq!(read.entries[0].reminders, 2);
    assert_eq!(facts.situation().unwrap().incidents.len(), 2);
}

fn question() -> Question {
    Question {
        scope: Scope::PullRequest(480),
        question: "Release this hold?".into(),
        options: vec![
            super::super::QuestionOption {
                label: "Keep".into(),
                detail: "Keep the hold".into(),
            },
            super::super::QuestionOption {
                label: "Release".into(),
                detail: "Request an explicit release".into(),
            },
        ],
        recommended: "Keep".into(),
        meanwhile: None,
        source: super::super::Source::Command {
            command: "ask 480".into(),
        },
    }
}
fn answer(ask: &Ask) -> super::super::AnswerRequest {
    super::super::AnswerRequest {
        ask_id: ask.id.clone(),
        question_revision: ask.revision,
        choice: super::super::Choice::Option("Release".into()),
        source: super::super::Source::Command {
            command: "decide Release".into(),
        },
    }
}

#[test]
fn answers_are_owner_bound_to_a_question_and_do_not_release_holds() {
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts.clone());
    standing.rule(0, Authority::Owner, 1, hold(true, None)).unwrap();
    let ask = standing.ask(1, 2, question()).unwrap();
    assert!(ask.meanwhile.is_none());
    assert_eq!(facts.situation().unwrap().asks[0].id, ask.id);
    assert!(standing.answer(2, Authority::Lead, 3, answer(&ask)).is_err());
    let mut stale = answer(&ask);
    stale.question_revision += 1;
    assert!(standing.answer(2, Authority::Owner, 3, stale).is_err());
    let mut invalid = answer(&ask);
    invalid.choice = super::super::Choice::Option("missing".into());
    assert!(standing.answer(2, Authority::Owner, 3, invalid).is_err());
    let result = standing.answer(2, Authority::Owner, 3, answer(&ask)).unwrap();
    assert!(result.answer.is_some());
    assert!(facts.situation().unwrap().asks.is_empty());
    assert!(standing.is_held(480).unwrap().is_some());
    assert!(standing
        .with_guard(Action::Enqueue, subject(), |_| Ok(()))
        .is_err());
    assert!(standing.answer(3, Authority::Owner, 4, answer(&ask)).is_err());
}

#[test]
fn an_ask_capacity_refusal_does_not_reserve_a_stuck_standing_revision() {
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts.clone());
    for index in 0..128 {
        facts
            .append(
                Checkpoint {
                    source: "runner".into(),
                    revision: index + 1,
                },
                1,
                Fact::TaskStarted(oxa_proto::work::Running {
                    task: format!("task-{index}"),
                    host: "helper".into(),
                    branch: format!("agent/task-{index}"),
                    worktree: "~/project".into(),
                    session_id: None,
                    step: oxa_proto::work::TaskStep::Preparing,
                }),
            )
            .unwrap();
    }
    assert!(standing.ask(0, 2, question()).is_err());
    assert_eq!(standing.read().unwrap().revision, 0);
    assert!(standing.read().unwrap().asks.is_empty());
    let document: Document = serde_json::from_slice(&standing.root.read(FILE, MAX_FILE).unwrap()).unwrap();
    assert!(document.pending.is_none());
    facts
        .append(
            Checkpoint {
                source: "runner".into(),
                revision: 129,
            },
            3,
            Fact::TaskTransition(oxa_proto::work::TaskTransition {
                task: "task-0".into(),
                step: oxa_proto::work::TaskStep::Finished,
                session_id: None,
                reason: Some(oxa_proto::work::StepReason::Interrupted),
            }),
        )
        .unwrap();
    let ask = standing.ask(0, 4, question()).unwrap();
    assert_eq!(facts.situation().unwrap().asks.len(), 1);
    standing.answer(1, Authority::Owner, 5, answer(&ask)).unwrap();
    assert!(facts.situation().unwrap().asks.is_empty());
}

#[test]
fn a_late_answer_is_retained_even_when_its_question_is_the_oldest() {
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts);
    let oldest = standing.ask(0, 100, question()).unwrap();
    let mut revision = 1;
    for _ in 0..16 {
        let ask = standing.ask(revision, 100, question()).unwrap();
        revision += 1;
        standing
            .answer(revision, Authority::Owner, 100, answer(&ask))
            .unwrap();
        revision += 1;
    }
    standing
        .answer(revision, Authority::Owner, 1, answer(&oldest))
        .unwrap();
    let read = standing.read().unwrap();
    assert!(read.history_truncated);
    assert_eq!(read.asks.len(), 16);
    assert!(read
        .asks
        .iter()
        .find(|ask| ask.id == oldest.id)
        .unwrap()
        .answer
        .is_some());
}
