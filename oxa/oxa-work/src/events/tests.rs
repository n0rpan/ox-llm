use super::*;
use oxa_proto::work::*;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let root = std::env::temp_dir().join(format!(
                "oxa-facts-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::DirBuilder::new().mode(0o700).create(&root) {
                Ok(()) => return Self(root),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create isolated fixture: {error}"),
            }
        }
    }
    fn root(&self) -> PathBuf {
        self.0.join("facts")
    }
    fn facts(&self) -> Facts {
        Facts::initialize(&self.root()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn checkpoint(source: &str, revision: u64) -> Checkpoint {
    Checkpoint {
        source: source.into(),
        revision,
    }
}
fn mechanical() -> Fact {
    Fact::Mechanical(Mechanical {
        subject: Subject::Repository,
        kind: MechanicalKind::TransientRetry,
    })
}
fn task(id: usize) -> Running {
    Running {
        task: format!("task-{id}"),
        host: "helper".into(),
        branch: format!("agent/task-{id}"),
        worktree: format!("~/project/task-{id}"),
        session_id: None,
        step: TaskStep::Preparing,
    }
}
fn reserve(facts: &Facts, fact: Fact) -> Event {
    let mut state: State = serde_json::from_slice(&facts.root.read(STATE, STATE_BYTES).unwrap()).unwrap();
    let event = Event {
        seq: state.last_seq + 1,
        at_ms: 20,
        checkpoint: checkpoint("runner", state.last_seq + 1),
        fact,
    };
    state.pending = Some(event_value(&event).unwrap());
    facts.root.atomic(STATE, &state.bytes().unwrap()).unwrap();
    event
}

#[test]
fn a_crashed_preparation_keeps_its_projection_capacity_and_commits_once() {
    const CHILD: &str = "OXA_FACTS_PREPARATION_CHILD";
    let observation = || {
        Fact::StandingChanged(StandingChanged {
            revision: 1,
            goal: None,
            ask_opened: Some(Ask {
                id: "prepared-ask".into(),
                subject: Subject::Repository,
                question: "Choose an option".into(),
            }),
            ask_closed: None,
            incident: None,
        })
    };
    if let Some(root) = std::env::var_os(CHILD) {
        let facts = Facts::open(Path::new(&root).join("facts").as_path()).unwrap();
        let policy = Root::open(Path::new(&root), false).unwrap();
        facts
            .append_prepared(checkpoint("standing", 1), 500, observation(), |event| {
                policy.atomic("prepared.json", &event.to_json().unwrap()).unwrap();
                std::process::exit(73);
            })
            .unwrap();
        panic!("preparation did not reach its crash boundary");
    }
    let fixture = Fixture::new();
    let facts = fixture.facts();
    for revision in 1..=127 {
        facts
            .append(
                checkpoint("runner", revision),
                revision,
                Fact::TaskStarted(task(revision as usize)),
            )
            .unwrap();
    }
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env(CHILD, &fixture.0)
        .args([
            "--exact",
            "events::tests::a_crashed_preparation_keeps_its_projection_capacity_and_commits_once",
        ])
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(73),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let recovered = Facts::open(&fixture.root()).unwrap();
    assert_eq!(recovered.situation().unwrap().as_of_seq, 127);
    assert!(recovered
        .append(checkpoint("runner", 128), 600, Fact::TaskStarted(task(128)))
        .is_err());
    assert_eq!(recovered.events_after(127, 64).unwrap().events.len(), 0);
    let committed = recovered
        .append_prepared(checkpoint("standing", 1), 500, observation(), |event| {
            assert_eq!(
                fs::read(fixture.0.join("prepared.json")).unwrap(),
                event.to_json().unwrap()
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(committed.seq, 128);
    assert_eq!(recovered.situation().unwrap().asks.len(), 1);
    let again = recovered
        .append_prepared(checkpoint("standing", 1), 500, observation(), |_| {
            panic!("prepared twice")
        })
        .unwrap();
    assert_eq!(again, committed);
    recovered
        .append(
            checkpoint("cleanup", 1),
            700,
            Fact::AskClosed(AskClosed {
                id: "prepared-ask".into(),
            }),
        )
        .unwrap();
    recovered
        .append(checkpoint("runner", 128), 800, Fact::TaskStarted(task(128)))
        .unwrap();
    assert_eq!(recovered.situation().unwrap().running.len(), 128);
}

#[test]
fn lifecycle_facts_do_not_turn_worker_success_into_landed_work() {
    let env = Fixture::new();
    let facts = env.facts();
    let marker = env.0.join("runner-state.json");
    fs::write(&marker, b"owned only by runner").unwrap();
    let pr = PullRequest {
        number: 123,
        head: "a".repeat(40),
    };
    let observations = [
        Fact::TaskStarted(task(1)),
        Fact::TaskTransition(TaskTransition {
            task: "task-1".into(),
            step: TaskStep::Worker,
            session_id: Some("saved".into()),
            reason: Some(StepReason::Prepared),
        }),
        Fact::WorkerFinished(WorkerFinished {
            task: "task-1".into(),
            session_id: Some("saved".into()),
            outcome: WorkerOutcome::Done,
            error_class: None,
        }),
        Fact::LocalAcceptance(LocalAcceptance {
            task: "task-1".into(),
            commit: pr.head.clone(),
            evidence: Evidence {
                kind: EvidenceKind::LocalCheck,
                id: "checks-1".into(),
            },
        }),
        Fact::Published(Publication {
            task: "task-1".into(),
            pr: pr.clone(),
        }),
        Fact::ExternalAcceptance(ExternalAcceptance {
            pr: pr.clone(),
            evidence: Evidence {
                kind: EvidenceKind::Gate,
                id: "pr-run-1".into(),
            },
        }),
    ];
    for (index, fact) in observations.into_iter().enumerate() {
        facts
            .append(checkpoint("runner", index as u64 + 1), index as u64, fact)
            .unwrap();
        assert!(facts.situation().unwrap().landed.is_empty());
    }
    assert_eq!(
        facts.situation().unwrap().running[0].session_id.as_deref(),
        Some("saved")
    );
    facts
        .append(
            checkpoint("runner", 7),
            7,
            Fact::PrMerged(Landed {
                pr,
                merge_commit: "b".repeat(40),
                merged_at_ms: 7,
            }),
        )
        .unwrap();
    let snapshot = facts.situation().unwrap();
    assert_eq!(snapshot.landed.len(), 1);
    assert!(snapshot.published.is_empty());
    assert!(snapshot.local_accepted.is_empty());
    facts
        .append(
            checkpoint("runner", 8),
            8,
            Fact::TaskTransition(TaskTransition {
                task: "task-1".into(),
                step: TaskStep::Finished,
                session_id: Some("saved".into()),
                reason: Some(StepReason::Saved),
            }),
        )
        .unwrap();
    assert!(facts.situation().unwrap().running.is_empty());
    assert_eq!(fs::read(marker).unwrap(), b"owned only by runner");
}

#[test]
fn lost_acknowledgements_deduplicate_across_restart_and_changed_clocks() {
    let env = Fixture::new();
    let facts = env.facts();
    let original = facts.append(checkpoint("runner", 1), 10, mechanical()).unwrap();
    let reopened = Facts::open(&env.root()).unwrap();
    assert_eq!(
        reopened
            .append(checkpoint("runner", 1), 999, mechanical())
            .unwrap(),
        original
    );
    assert!(reopened
        .append(
            checkpoint("runner", 1),
            999,
            Fact::Enqueued(PullRequest {
                number: 123,
                head: "a".repeat(40)
            })
        )
        .is_err());
    reopened
        .append(checkpoint("runner", 2), 20, mechanical())
        .unwrap();
    assert!(reopened
        .append(checkpoint("runner", 1), 10, mechanical())
        .is_err());
    assert_eq!(reopened.events_after(0, 64).unwrap().events.len(), 2);
}

#[test]
fn repository_revision_gaps_do_not_create_event_gaps_or_per_task_tombstones() {
    let env = Fixture::new();
    let facts = env.facts();
    for revision in [3, 8, 100] {
        facts
            .append(checkpoint("runner", revision), revision, mechanical())
            .unwrap();
    }
    let page = facts.events_after(0, 64).unwrap();
    assert_eq!(
        page.events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let state: State = serde_json::from_slice(&facts.root.read(STATE, STATE_BYTES).unwrap()).unwrap();
    assert_eq!(state.last_by_source.len(), 1);
}

#[test]
fn a_reserved_torn_line_is_replayed_once_and_the_situation_catches_up() {
    let env = Fixture::new();
    let facts = env.facts();
    facts.append(checkpoint("runner", 1), 10, mechanical()).unwrap();
    let pending = reserve(&facts, mechanical());
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(env.root().join(CURRENT))
        .unwrap();
    file.write_all(b"{\"type\":\"oxa_work_event\"").unwrap();
    file.sync_all().unwrap();
    let recovered = Facts::open(&env.root()).unwrap();
    assert_eq!(recovered.situation().unwrap().as_of_seq, 2);
    assert_eq!(
        recovered
            .append(pending.checkpoint.clone(), 999, pending.fact.clone())
            .unwrap(),
        pending
    );
    assert_eq!(recovered.events_after(0, 64).unwrap().events.len(), 2);
    assert!(fs::read(env.root().join(CURRENT)).unwrap().ends_with(b"\n"));
}

#[test]
fn an_appended_pending_fact_and_a_lagging_snapshot_recover_without_another_line() {
    for committed in [false, true] {
        let env = Fixture::new();
        let facts = env.facts();
        facts.append(checkpoint("runner", 1), 10, mechanical()).unwrap();
        let pending = reserve(&facts, mechanical());
        let mut bytes = pending.to_json().unwrap();
        bytes.push(b'\n');
        facts.root.append(CURRENT, &bytes).unwrap();
        if committed {
            let mut state: State =
                serde_json::from_slice(&facts.root.read(STATE, STATE_BYTES).unwrap()).unwrap();
            state.committed(&pending).unwrap();
            facts.root.atomic(STATE, &state.bytes().unwrap()).unwrap();
        }
        let recovered = Facts::open(&env.root()).unwrap();
        assert_eq!(recovered.situation().unwrap().as_of_seq, 2);
        assert_eq!(recovered.events_after(0, 64).unwrap().events.len(), 2);
    }
}

#[test]
fn pending_recovery_cannot_commit_without_opening_the_journal_for_sync() {
    // Root bypasses the real journal permission failure; run this case as an ordinary user.
    // SAFETY: geteuid has no arguments, pointers or failure mode.
    if unsafe { libc::geteuid() } == 0 {
        use std::os::unix::process::CommandExt;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env_clear()
            .args([
                "--exact",
                "events::tests::pending_recovery_cannot_commit_without_opening_the_journal_for_sync",
            ])
            .uid(65534)
            .gid(65534)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let env = Fixture::new();
    let facts = env.facts();
    let pending = reserve(&facts, mechanical());
    let mut bytes = pending.to_json().unwrap();
    bytes.push(b'\n');
    // The complete line is visible without having been synced by its writer.
    fs::OpenOptions::new()
        .append(true)
        .open(env.root().join(CURRENT))
        .unwrap()
        .write_all(&bytes)
        .unwrap();
    let before = facts.root.read(STATE, STATE_BYTES).unwrap();
    let snapshot = facts.root.read(SITUATION, MAX_SNAPSHOT_BYTES).unwrap();
    fs::set_permissions(env.root().join(CURRENT), fs::Permissions::from_mode(0o400)).unwrap();
    let recovered = Facts::open(&env.root());
    fs::set_permissions(env.root().join(CURRENT), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(recovered.is_err(), "unsynced pending line was acknowledged");
    assert_eq!(facts.root.read(STATE, STATE_BYTES).unwrap(), before);
    assert_eq!(facts.root.read(SITUATION, MAX_SNAPSHOT_BYTES).unwrap(), snapshot);
    let recovered = Facts::open(&env.root()).unwrap();
    assert_eq!(recovered.situation().unwrap().as_of_seq, pending.seq);
    assert_eq!(recovered.events_after(0, 64).unwrap().events, [pending]);
}

#[test]
fn interruption_between_rotation_and_new_current_file_recovers_the_reserved_event() {
    let env = Fixture::new();
    let facts = env.facts();
    facts.append(checkpoint("runner", 1), 10, mechanical()).unwrap();
    reserve(&facts, mechanical());
    fs::rename(env.root().join(CURRENT), env.root().join(PREVIOUS)).unwrap();
    let recovered = Facts::open(&env.root()).unwrap();
    assert_eq!(
        recovered
            .events_after(0, 64)
            .unwrap()
            .events
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn rotation_is_bounded_and_cursor_gaps_are_explicit_without_resetting_sequences() {
    let env = Fixture::new();
    let mut facts = env.facts();
    facts.journal_bytes = 1024;
    let quiet = facts.append(checkpoint("quiet", 1), 0, mechanical()).unwrap();
    for revision in 1..=40 {
        facts
            .append(checkpoint("busy", revision), revision, mechanical())
            .unwrap();
    }
    for leaf in [CURRENT, PREVIOUS] {
        assert!(fs::metadata(env.root().join(leaf)).unwrap().len() <= 1024);
    }
    let page = facts.events_after(0, 64).unwrap();
    assert!(page.gap && page.first_available > 1);
    assert_eq!(page.events[0].seq, page.first_available);
    assert_eq!(page.next, 41);
    assert_eq!(
        facts.append(checkpoint("quiet", 1), 999, mechanical()).unwrap(),
        quiet
    );
    assert_eq!(facts.situation().unwrap().as_of_seq, 41);
    assert!(facts.events_after(42, 64).is_err());
    assert!(facts.events_after(0, 0).is_err());
}

#[test]
fn large_valid_events_page_by_bytes_and_keep_advancing_across_retention_gaps() {
    let fixture = Fixture::new();
    let facts = fixture.facts();
    let observation = |revision| {
        Fact::StandingChanged(StandingChanged {
            revision,
            goal: Some("\"".repeat(1024)),
            ask_opened: Some(Ask {
                id: "ask".repeat(32),
                subject: Subject::Repository,
                question: "\"".repeat(512),
            }),
            ask_closed: None,
            incident: Some(Incident {
                id: "incident".repeat(32),
                subject: Subject::Task("t".repeat(128)),
                kind: IncidentKind::RuleReminder,
                evidence: Some(Evidence {
                    kind: EvidenceKind::LocalCheck,
                    id: "e".repeat(256),
                }),
            }),
        })
    };
    for revision in 1..=64 {
        let event = facts
            .append(checkpoint("standing", revision), revision, observation(revision))
            .unwrap();
        assert!(event.to_json().unwrap().len() <= MAX_EVENT_BYTES);
    }
    let first = facts
        .events_after(0, 64)
        .expect("healthy large events must return a usable bounded page");
    assert!(!first.gap);
    assert!(!first.events.is_empty() && first.events.len() < 64);
    let mut cursor = 0;
    let mut seen = Vec::new();
    while cursor < 64 {
        let page = facts.events_after(cursor, 64).unwrap();
        assert!(page.to_json().unwrap().len() <= oxa_proto::work::MAX_PAGE_BYTES);
        assert_eq!(page.next, page.events.last().unwrap().seq);
        assert!(page.next > cursor);
        seen.extend(page.events.iter().map(|event| event.seq));
        cursor = page.next;
    }
    assert_eq!(seen, (1..=64).collect::<Vec<_>>());
    for revision in 65..=180 {
        facts
            .append(checkpoint("standing", revision), revision, observation(revision))
            .unwrap();
    }
    let page = facts.events_after(0, 64).unwrap();
    assert!(page.gap && page.first_available > 1);
    let first_retained = page.first_available;
    cursor = first_retained - 1;
    seen.clear();
    while cursor < 180 {
        let page = facts.events_after(cursor, 64).unwrap();
        assert!(page.to_json().unwrap().len() <= oxa_proto::work::MAX_PAGE_BYTES);
        assert!(page.next > cursor);
        assert_eq!(page.next, page.events.last().unwrap().seq);
        seen.extend(page.events.iter().map(|event| event.seq));
        cursor = page.next;
    }
    assert_eq!(seen, (first_retained..=180).collect::<Vec<_>>());
    assert!(facts.events_after(180, 64).unwrap().events.is_empty());
}

#[test]
fn concurrent_sources_get_unique_contiguous_repository_sequences() {
    let env = Fixture::new();
    let facts = env.facts();
    std::thread::scope(|scope| {
        for source in ["one", "two", "three", "four"] {
            let facts = &facts;
            scope.spawn(move || {
                for revision in 1..=8 {
                    facts
                        .append(checkpoint(source, revision), revision, mechanical())
                        .unwrap();
                }
            });
        }
    });
    let page = facts.events_after(0, 64).unwrap();
    assert_eq!(
        page.events.iter().map(|event| event.seq).collect::<Vec<_>>(),
        (1..=32).collect::<Vec<_>>()
    );
}

#[test]
fn active_capacity_is_an_error_and_only_recent_terminal_history_is_trimmed() {
    let env = Fixture::new();
    let facts = env.facts();
    for id in 0..MAX_ITEMS {
        facts
            .append(
                checkpoint("runner", id as u64 + 1),
                0,
                Fact::TaskStarted(task(id)),
            )
            .unwrap();
    }
    assert!(facts
        .append(
            checkpoint("runner", MAX_ITEMS as u64 + 1),
            0,
            Fact::TaskStarted(task(MAX_ITEMS))
        )
        .is_err());
    assert_eq!(facts.situation().unwrap().running.len(), MAX_ITEMS);
    assert_eq!(facts.situation().unwrap().as_of_seq, MAX_ITEMS as u64);
    let history = Fixture::new();
    let facts = history.facts();
    for id in 1..=20 {
        facts
            .append(
                checkpoint("runner", id),
                id,
                Fact::Incident(Incident {
                    id: format!("incident-{id}"),
                    subject: Subject::Repository,
                    kind: IncidentKind::NoBuildRequeue,
                    evidence: None,
                }),
            )
            .unwrap();
    }
    let snapshot = facts.situation().unwrap();
    assert!(snapshot.history_truncated);
    assert_eq!(snapshot.incidents.len(), 16);
    assert_eq!(crate::incidents::recent(&facts).unwrap()[0].id, "incident-5");
}

#[test]
fn byte_capacity_trims_history_before_refusing_active_asks() {
    let env = Fixture::new();
    let facts = env.facts();
    for id in 0..16 {
        facts
            .append(
                checkpoint("runner", id + 1),
                0,
                Fact::Incident(Incident {
                    id: format!("{id:016}{}", "i".repeat(240)),
                    subject: Subject::Task("t".repeat(128)),
                    kind: IncidentKind::NoBuildRequeue,
                    evidence: Some(Evidence {
                        kind: EvidenceKind::Gate,
                        id: "e".repeat(256),
                    }),
                }),
            )
            .unwrap();
    }
    for id in 0..95 {
        facts
            .append(
                checkpoint("runner", id + 17),
                0,
                Fact::AskOpened(Ask {
                    id: format!("ask-{id}"),
                    subject: Subject::Repository,
                    question: "q".repeat(512),
                }),
            )
            .unwrap();
    }
    let snapshot = facts.situation().unwrap();
    assert_eq!(snapshot.as_of_seq, 111);
    assert_eq!(snapshot.asks.len(), 95);
    assert!(snapshot.history_truncated);
    assert!(snapshot.incidents.len() < 16);
    assert!(snapshot.to_json().unwrap().len() <= MAX_SNAPSHOT_BYTES);
    assert_eq!(Facts::open(&env.root()).unwrap().situation().unwrap(), snapshot);
    for id in 95..MAX_ITEMS {
        let before = facts.situation().unwrap();
        let result = facts.append(
            checkpoint("runner", id as u64 + 17),
            0,
            Fact::AskOpened(Ask {
                id: format!("ask-{id}"),
                subject: Subject::Repository,
                question: "q".repeat(512),
            }),
        );
        if result.is_err() {
            assert!(before.incidents.is_empty());
            assert_eq!(facts.situation().unwrap(), before);
            return;
        }
        assert_eq!(facts.situation().unwrap().asks.len(), id + 1);
    }
    panic!("active asks alone must reach the byte limit before the row limit");
}

#[test]
fn parent_directory_sync_error_blocks_initialization() {
    const CHILD_ROOT: &str = "OXA_FACTS_TEST_UNREADABLE_PARENT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let error = Facts::initialize(Path::new(&root)).expect_err("parent cannot be synced");
        assert!(error.contains("directory"), "{error}");
        return;
    }
    let env = Fixture::new();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.env_clear().env(CHILD_ROOT, env.root()).args([
        "--exact",
        "events::tests::parent_directory_sync_error_blocks_initialization",
    ]);
    // Root can read mode 0300. Exercise the same real permission failure as a normal user.
    // SAFETY: geteuid has no arguments, pointers or failure mode.
    if unsafe { libc::geteuid() } == 0 {
        use std::os::unix::process::CommandExt;
        std::os::unix::fs::chown(&env.0, Some(65534), Some(65534)).unwrap();
        child.uid(65534).gid(65534);
    }
    fs::set_permissions(&env.0, fs::Permissions::from_mode(0o300)).unwrap();
    let output = child.output();
    fs::set_permissions(&env.0, fs::Permissions::from_mode(0o700)).unwrap();
    let output = output.unwrap();
    assert!(
        output.status.success(),
        "child failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn head_changes_and_unrelated_facts_do_not_silently_clear_blocks_or_asks() {
    let env = Fixture::new();
    let facts = env.facts();
    let subject = Subject::PullRequest(123);
    facts
        .append(
            checkpoint("runner", 1),
            1,
            Fact::Blocked(Blocked {
                subject: subject.clone(),
                code: "held".into(),
                ask_id: Some("ask-1".into()),
            }),
        )
        .unwrap();
    facts
        .append(
            checkpoint("runner", 2),
            2,
            Fact::AskOpened(Ask {
                id: "ask-1".into(),
                subject: subject.clone(),
                question: "Release hold?".into(),
            }),
        )
        .unwrap();
    facts
        .append(
            checkpoint("runner", 3),
            3,
            Fact::Published(Publication {
                task: "task-1".into(),
                pr: PullRequest {
                    number: 123,
                    head: "b".repeat(40),
                },
            }),
        )
        .unwrap();
    let snapshot = facts.situation().unwrap();
    assert_eq!(snapshot.blocked.len(), 1);
    assert_eq!(snapshot.asks.len(), 1);
    facts
        .append(checkpoint("runner", 4), 4, Fact::Unblocked(subject))
        .unwrap();
    facts
        .append(
            checkpoint("runner", 5),
            5,
            Fact::AskClosed(AskClosed { id: "ask-1".into() }),
        )
        .unwrap();
    let snapshot = facts.situation().unwrap();
    assert!(snapshot.blocked.is_empty() && snapshot.asks.is_empty());
}

#[test]
fn missing_corrupt_and_unreserved_torn_files_fail_instead_of_becoming_empty() {
    for leaf in [STATE, SITUATION, CURRENT] {
        let env = Fixture::new();
        let _facts = env.facts();
        fs::remove_file(env.root().join(leaf)).unwrap();
        assert!(Facts::open(&env.root()).is_err());
        assert!(Facts::initialize(&env.root()).is_err());
    }
    for leaf in [STATE, SITUATION, CURRENT] {
        let env = Fixture::new();
        let _facts = env.facts();
        fs::write(env.root().join(leaf), b"PRIVATE_MARKER").unwrap();
        let error = Facts::open(&env.root()).unwrap_err();
        assert!(!error.contains("PRIVATE_MARKER"));
    }
}

#[test]
fn poisoned_leaves_and_root_symlinks_never_read_or_replace_the_target() {
    for leaf in [STATE, SITUATION, CURRENT, LOCK] {
        let env = Fixture::new();
        let _facts = env.facts();
        let target = env.0.join("outside");
        fs::write(&target, b"keep this target").unwrap();
        fs::remove_file(env.root().join(leaf)).unwrap();
        std::os::unix::fs::symlink(&target, env.root().join(leaf)).unwrap();
        assert!(Facts::open(&env.root()).is_err());
        assert_eq!(fs::read(target).unwrap(), b"keep this target");
    }
    let env = Fixture::new();
    let target = env.0.join("outside");
    fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, env.root()).unwrap();
    assert!(Facts::initialize(&env.root()).is_err());
    assert!(fs::read_dir(target).unwrap().next().is_none());
}

#[test]
fn artifacts_are_private_and_source_capacity_does_not_consume_a_sequence() {
    let env = Fixture::new();
    let facts = env.facts();
    assert_eq!(
        fs::metadata(env.root()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for leaf in [STATE, SITUATION, CURRENT, LOCK] {
        assert_eq!(
            fs::metadata(env.root().join(leaf)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    for source in 0..MAX_SOURCES {
        facts
            .append(checkpoint(&format!("source-{source}"), 1), 0, mechanical())
            .unwrap();
    }
    assert!(facts.append(checkpoint("too-many", 1), 0, mechanical()).is_err());
    assert_eq!(facts.situation().unwrap().as_of_seq, MAX_SOURCES as u64);
}
