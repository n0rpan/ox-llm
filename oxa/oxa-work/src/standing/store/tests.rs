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
        value: Value::Constraint { active, until: None },
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
            said_by: Authority::Owner,
            source: request.source,
            supersedes: None,
            state: EntryState::Stored,
            reminders: 0,
            last_reminder: None,
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
    assert!(!standing
        .remind(
            1,
            Authority::Owner,
            &note.id,
            2,
            super::super::Source::Command {
                command: "remind".into()
            }
        )
        .unwrap()
        .needs_conversion_offer());
    assert!(standing
        .remind(
            2,
            Authority::Owner,
            &note.id,
            3,
            super::super::Source::Command {
                command: "remind".into()
            }
        )
        .unwrap()
        .needs_conversion_offer());
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
        said: "Release".into(),
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

#[test]
fn a_failed_fact_reservation_cannot_strand_policy_at_projection_capacity() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid takes no arguments and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        use std::os::unix::process::CommandExt;
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .args([
                "--exact",
                "standing::store::tests::a_failed_fact_reservation_cannot_strand_policy_at_projection_capacity",
            ])
            .uid(65534)
            .gid(65534)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts.clone());
    standing.rule(0, Authority::Owner, 1, hold(true, None)).unwrap();
    let existing = standing.ask(1, 2, question()).unwrap();
    for revision in 1..=126 {
        facts
            .append(
                Checkpoint {
                    source: "runner".into(),
                    revision,
                },
                revision + 10,
                Fact::TaskStarted(oxa_proto::work::Running {
                    task: format!("busy-{revision}"),
                    host: "local".into(),
                    branch: format!("agent/busy-{revision}"),
                    worktree: "/tmp/synthetic".into(),
                    session_id: None,
                    step: oxa_proto::work::TaskStep::Preparing,
                }),
            )
            .unwrap();
    }
    let root = fixture.0.join("facts");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).unwrap();
    let failed = standing.ask(2, 500, question());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(failed.is_err(), "facts reservation unexpectedly succeeded");
    facts
        .append(
            Checkpoint {
                source: "runner".into(),
                revision: 127,
            },
            600,
            Fact::TaskStarted(oxa_proto::work::Running {
                task: "last-slot".into(),
                host: "local".into(),
                branch: "agent/last-slot".into(),
                worktree: "/tmp/synthetic".into(),
                session_id: None,
                step: oxa_proto::work::TaskStep::Preparing,
            }),
        )
        .unwrap();
    let read = standing
        .read()
        .expect("a failed reservation stranded every standing operation");
    assert_eq!(read.revision, 2);
    assert_eq!(read.open_asks().count(), 1);
    standing
        .answer(2, Authority::Owner, 700, answer(&existing))
        .unwrap();
    assert!(standing.read().unwrap().open_asks().next().is_none());
    assert_eq!(facts.situation().unwrap().running.len(), 127);
    assert!(matches!(
        standing.with_guard(Action::Update, subject(), |_| Ok(())),
        Err(GuardError::Refused(_))
    ));
}

#[test]
fn recovery_syncs_the_prepared_policy_before_committing_its_fact() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no arguments, pointers or failure mode.
    if unsafe { libc::geteuid() } == 0 {
        use std::os::unix::process::CommandExt;
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .args([
                "--exact",
                "standing::store::tests::recovery_syncs_the_prepared_policy_before_committing_its_fact",
            ])
            .uid(65534)
            .gid(65534)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
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
        said_by: Authority::Owner,
        source: request.source,
        supersedes: None,
        state: EntryState::Stored,
        reminders: 0,
        last_reminder: None,
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
    let document = Document {
        version: 1,
        current: before,
        pending: Some(Pending {
            after,
            observation: serde_json::from_slice(&event.to_json().unwrap()).unwrap(),
        }),
    };
    standing.root.atomic(FILE, &document.bytes().unwrap()).unwrap();
    let path = fixture.0.join("standing/standing.json");
    let original = fs::read(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
    let recovered = standing.read();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        recovered.is_err(),
        "policy was acknowledged before its prepared data could be synced"
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(facts.situation().unwrap().standing_revision, 0);
    assert_eq!(standing.read().unwrap().revision, 1);
    assert_eq!(facts.events_after(0, 64).unwrap().events.len(), 1);
}

#[test]
fn a_concurrent_owner_hold_waits_for_one_effect_then_blocks_the_next() {
    const CHILD_ROOT: &str = "OXA_STANDING_GUARD_CHILD";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let facts = Facts::open(&root.join("facts")).unwrap();
        fs::write(root.join("child-ready"), b"ready").unwrap();
        let standing = Standing::open(&root.join("standing"), facts).unwrap();
        standing.rule(0, Authority::Owner, 1, hold(true, None)).unwrap();
        return;
    }
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts);
    let mut child = None;
    let completed = standing.with_guard(Action::Update, subject(), |read| {
        let process = std::process::Command::new(std::env::current_exe().unwrap())
            .env_clear().env(CHILD_ROOT, &fixture.0)
            .args(["--exact", "standing::store::tests::a_concurrent_owner_hold_waits_for_one_effect_then_blocks_the_next"])
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        child = Some(Child(process));
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !fixture.0.join("child-ready").exists() {
            assert!(std::time::Instant::now() < until, "child failed to start");
            assert!(child.as_mut().unwrap().0.try_wait().unwrap().is_none());
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(child.as_mut().unwrap().0.try_wait().unwrap().is_none(), "hold writer passed the live effect guard");
        let document: Document = serde_json::from_slice(&standing.root.read(FILE, MAX_FILE).unwrap()).unwrap();
        assert_eq!(document.current.revision, read.revision);
        Ok(read.revision)
    }).unwrap();
    assert_eq!(completed.result.unwrap(), 0);
    let child = child.as_mut().unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "hold writer did not acquire the released guard"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(standing
        .with_guard(Action::Update, subject(), |_| Ok(()))
        .is_err());
}

fn policy(kind: Kind, scope: Scope, value: Value, supersedes: Option<&str>) -> Rule {
    Rule {
        kind,
        scope,
        value,
        said: "Synthetic owner rule".into(),
        source: super::super::Source::Command {
            command: "rule".into(),
        },
        supersedes: supersedes.map(str::to_owned),
    }
}

#[test]
fn budget_increases_gate_weakening_and_reserves_require_owner_authority() {
    use super::super::{GateEvidence as E, ReservedClass, ReviewTier as T};
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts);
    let host = Scope::Host("helper".into());
    let first = standing
        .rule(
            0,
            Authority::Owner,
            1,
            policy(
                Kind::Budget,
                host.clone(),
                Value::Budget {
                    daily_usd_micros: 200_000_000,
                },
                None,
            ),
        )
        .unwrap();
    let lower = standing
        .rule(
            1,
            Authority::Lead,
            2,
            policy(
                Kind::Budget,
                host.clone(),
                Value::Budget {
                    daily_usd_micros: 100_000_000,
                },
                Some(&first.id),
            ),
        )
        .unwrap();
    assert!(standing
        .rule(
            2,
            Authority::Lead,
            3,
            policy(
                Kind::Budget,
                host.clone(),
                Value::Budget {
                    daily_usd_micros: 150_000_000
                },
                Some(&lower.id)
            )
        )
        .is_err());
    assert!(standing
        .rule(
            2,
            Authority::Lead,
            3,
            policy(Kind::Budget, host, Value::Retired, Some(&lower.id))
        )
        .is_err());
    let path = Scope::Path("engine/**".into());
    let gate = standing
        .rule(
            2,
            Authority::Owner,
            3,
            policy(
                Kind::Gate,
                path.clone(),
                Value::Gate {
                    tier: T::T2,
                    evidence: vec![E::LocalChecks, E::PrBuild],
                },
                None,
            ),
        )
        .unwrap();
    let stronger = standing
        .rule(
            3,
            Authority::Lead,
            4,
            policy(
                Kind::Gate,
                path.clone(),
                Value::Gate {
                    tier: T::T3,
                    evidence: vec![E::LocalChecks, E::PrBuild, E::HardwareMeasurement],
                },
                Some(&gate.id),
            ),
        )
        .unwrap();
    for value in [
        Value::Gate {
            tier: T::T2,
            evidence: vec![E::LocalChecks, E::PrBuild, E::HardwareMeasurement],
        },
        Value::Gate {
            tier: T::T3,
            evidence: vec![E::LocalChecks, E::PrBuild],
        },
        Value::Retired,
    ] {
        assert!(standing
            .rule(
                4,
                Authority::Lead,
                5,
                policy(Kind::Gate, path.clone(), value, Some(&stronger.id))
            )
            .is_err());
    }
    let reserve = policy(
        Kind::Reserve,
        Scope::Repository,
        Value::Reserve {
            classes: vec![ReservedClass::Paid],
        },
        None,
    );
    assert!(standing.rule(4, Authority::Lead, 5, reserve.clone()).is_err());
    let reserve = standing.rule(4, Authority::Owner, 5, reserve).unwrap();
    assert!(standing
        .rule(
            5,
            Authority::Lead,
            6,
            policy(
                Kind::Reserve,
                Scope::Repository,
                Value::Retired,
                Some(&reserve.id)
            )
        )
        .is_err());
    assert_eq!(standing.read().unwrap().revision, 5);
}

#[test]
fn only_accepting_the_bound_proposal_applies_its_exact_rule() {
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts.clone());
    let prior = standing.rule(0, Authority::Owner, 1, hold(true, None)).unwrap();
    let ask = standing
        .propose(1, 2, hold(false, Some(&prior.id)), question(), "Release".into())
        .unwrap();
    assert!(standing.is_held(480).unwrap().is_some());
    assert!(standing.answer(2, Authority::Lead, 3, answer(&ask)).is_err());
    let mut accepted = answer(&ask);
    accepted.said = "Yes, release this hold".into();
    standing.answer(2, Authority::Owner, 3, accepted).unwrap();
    let read = standing.read().unwrap();
    let released = read.entries.iter().find(|entry| entry.id == "R-3").unwrap();
    assert_eq!(released.said, "Yes, release this hold");
    assert_eq!(
        released.value,
        Value::Constraint {
            active: false,
            until: None
        }
    );
    assert_eq!(released.state, EntryState::Stored);
    assert!(standing.is_held(480).unwrap().is_none());
    assert_eq!(facts.situation().unwrap().standing_revision, 3);
    assert!(facts.situation().unwrap().asks.is_empty());
}

#[test]
fn a_proposal_cannot_replace_a_rule_changed_after_the_question() {
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts);
    let prior = standing.rule(0, Authority::Owner, 1, hold(true, None)).unwrap();
    let ask = standing
        .propose(1, 2, hold(false, Some(&prior.id)), question(), "Release".into())
        .unwrap();
    let current = standing
        .rule(2, Authority::Owner, 3, hold(true, Some(&prior.id)))
        .unwrap();
    assert!(standing.answer(3, Authority::Owner, 4, answer(&ask)).is_err());
    let read = standing.read().unwrap();
    assert_eq!(read.revision, 3);
    assert!(read.asks[0].answer.is_none());
    assert_eq!(standing.is_held(480).unwrap(), Some(current.id));
}

#[test]
fn an_advisory_conversion_requires_an_explicit_owner_decision() {
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts);
    let note = standing
        .rule(
            0,
            Authority::Owner,
            1,
            policy(
                Kind::Note,
                Scope::PullRequest(480),
                Value::Text("Pause this PR".into()),
                None,
            ),
        )
        .unwrap();
    let source = super::super::Source::Command {
        command: "remind".into(),
    };
    assert!(standing
        .remind(1, Authority::Lead, &note.id, 2, source.clone())
        .is_err());
    standing
        .remind(1, Authority::Owner, &note.id, 2, source.clone())
        .unwrap();
    let reminded = standing
        .remind(2, Authority::Owner, &note.id, 3, source.clone())
        .unwrap();
    assert!(reminded.needs_conversion_offer());
    assert_eq!(reminded.last_reminder.unwrap().source, source);
    assert!(standing.is_held(480).unwrap().is_none());
    let conversion = hold(true, Some(&note.id));
    assert!(standing.rule(3, Authority::Lead, 4, conversion.clone()).is_err());
    let mut prompt = question();
    prompt.question = "Enforce the advisory pause?".into();
    prompt.options[0].label = "Keep advisory".into();
    prompt.options[0].detail = "Leave the note advisory".into();
    prompt.options[1].label = "Enforce".into();
    prompt.options[1].detail = "Create the exact PR hold shown in this proposal".into();
    prompt.recommended = "Keep advisory".into();
    let ask = standing
        .propose(3, 4, conversion, prompt, "Enforce".into())
        .unwrap();
    let mut accepted = answer(&ask);
    accepted.choice = super::super::Choice::Option("Enforce".into());
    accepted.said = "Enforce".into();
    standing.answer(4, Authority::Owner, 5, accepted).unwrap();
    let read = standing.read().unwrap();
    assert!(!read.live_rules().any(|entry| entry.kind == Kind::Note));
    assert!(standing.is_held(480).unwrap().is_some());
}

#[test]
fn a_named_release_needs_the_matching_durable_merge_observation() {
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let standing = fixture.standing(facts.clone());
    let mut request = hold(true, None);
    request.said = "Hold until PR 478 is merged".into();
    request.value = Value::Constraint {
        active: true,
        until: Some(super::super::Condition::PrMerged(478)),
    };
    let entry = standing.rule(0, Authority::Owner, 1, request).unwrap();
    let mut changed = hold(true, Some(&entry.id));
    assert!(standing.rule(1, Authority::Lead, 2, changed.clone()).is_err());
    changed.value = Value::Constraint {
        active: true,
        until: Some(super::super::Condition::PrMerged(1)),
    };
    assert!(standing.rule(1, Authority::Lead, 2, changed).is_err());
    assert!(standing
        .release_when(1, &entry.id, 2, |_| Ok(None))
        .unwrap()
        .is_none());
    let proof = Event {
        seq: 2,
        at_ms: 3,
        checkpoint: Checkpoint {
            source: "github".into(),
            revision: 1,
        },
        fact: Fact::PrMerged(oxa_proto::work::Landed {
            pr: oxa_proto::work::PullRequest {
                number: 478,
                head: "a".repeat(40),
            },
            merge_commit: "b".repeat(40),
            merged_at_ms: 3,
        }),
    };
    assert!(standing
        .release_when(1, &entry.id, 3, |_| Ok(Some(proof.clone())))
        .is_err());
    assert!(standing.is_held(480).unwrap().is_some());
    let observed = facts
        .append(proof.checkpoint.clone(), proof.at_ms, proof.fact.clone())
        .unwrap();
    assert_eq!(observed, proof);
    let release = standing
        .release_when(1, &entry.id, 4, |condition| {
            assert_eq!(*condition, super::super::Condition::PrMerged(478));
            Ok(Some(observed.clone()))
        })
        .unwrap()
        .unwrap();
    assert!(matches!(release.state, EntryState::Applied { .. }));
    assert_eq!(release.said, "Hold until PR 478 is merged");
    assert!(standing.is_held(480).unwrap().is_none());
    assert_eq!(facts.situation().unwrap().landed[0].pr.number, 478);
}
