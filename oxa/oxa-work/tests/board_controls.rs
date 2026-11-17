use oxa_work::board::assignment::{Control, ManualAuthority, Pin, Role, Worker, WorkerKind};
use oxa_work::board::*;
use std::slice::from_ref;
use std::{collections::BTreeMap, fs, os::unix::fs::DirBuilderExt, path::PathBuf};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "oxa-board-controls-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn store(&self) -> Claims {
        Claims::open(&self.0).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
struct FakeGitHub(usize);
impl GitHub for FakeGitHub {
    fn open_pull_requests(&mut self) -> oxa_work::Result<Vec<PullRequest>> {
        self.0 += 1;
        Ok((1..=3)
            .map(|number| PullRequest {
                number,
                head: "synthetic-head".into(),
                release_pinned: number == 3,
                findings: true,
                required_ci: RequiredCi::Passed,
                mergeability: Mergeability::Mergeable,
                body: String::new(),
                removes_source: false,
                draft: false,
                reviewed: false,
            })
            .collect())
    }
}
fn policy() -> Policy {
    Policy {
        enabled: true,
        ..Policy::default()
    }
}
fn worker(slot: &str) -> Worker {
    Worker {
        id: format!("codex:node/{slot}"),
        kind: WorkerKind::Codex,
        skills: Default::default(),
        roles: [Role::Builder].into(),
    }
}
fn entry() -> Entry {
    Board::read(&mut FakeGitHub(0), &policy(), &BTreeMap::new(), 0)
        .unwrap()
        .entries
        .remove(0)
}

#[test]
fn controls_require_authority_and_compare_revision_after_reopen() {
    let fixture = Fixture::new();
    assert!(fixture
        .store()
        .control(3, None, 0, Control::OwnerOnly(true))
        .is_err());
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
    fixture
        .store()
        .control(3, Some(ManualAuthority::Lead), 0, Control::OwnerOnly(true))
        .unwrap();
    assert!(fixture
        .store()
        .control(3, Some(ManualAuthority::Owner), 0, Control::OwnerOnly(false))
        .is_err());
    let rows = fixture.store().assignments().unwrap();
    assert_eq!(rows[&3].data.revision, 1);
    assert!(rows[&3].data.policy.as_ref().unwrap().owner_only);
    assert_eq!(rows[&3].state.attempts, 0);
    assert_eq!(rows[&3].assignee(10), None);
}

#[test]
fn stale_entries_and_legacy_claims_cannot_bypass_persistent_controls() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let entry = entry();
    let worker = worker("slot-1");
    store
        .control(
            3,
            Some(ManualAuthority::Owner),
            0,
            Control::Pin(Pin::Worker(worker.id.clone())),
        )
        .unwrap();
    assert!(store
        .claim_for(&entry, &policy(), &crate::worker("slot-2"), 10, 10)
        .unwrap()
        .is_none());
    assert!(store
        .claim(&entry, &policy(), &worker.id, 10, 10)
        .unwrap()
        .is_none());
    let lease = store
        .claim_for(&entry, &policy(), &worker, 10, 10)
        .unwrap()
        .unwrap();
    store
        .control(3, Some(ManualAuthority::Lead), 2, Control::OwnerOnly(true))
        .unwrap();
    assert_eq!(
        store.assignments().unwrap()[&3].assignee(19),
        Some(worker.id.as_str())
    );
    assert_eq!(store.assignments().unwrap()[&3].assignee(20), None);
    assert!(store.release(3, &lease).unwrap());
    assert!(store
        .claim_for(&entry, &policy(), &worker, 21, 10)
        .unwrap()
        .is_none());
    let owner = Worker {
        roles: [Role::Owner].into(),
        ..worker
    };
    assert!(store
        .claim_for(&entry, &policy(), &owner, 21, 10)
        .unwrap()
        .is_some());
    assert_eq!(store.snapshot().unwrap()[&3].attempts, 2);
}

#[test]
fn one_snapshot_serves_all_workers_with_release_and_manual_pin_priority() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let first = worker("slot-1");
    let second = worker("slot-2");
    store
        .control(
            2,
            Some(ManualAuthority::Owner),
            0,
            Control::Pin(Pin::Worker(first.id.clone())),
        )
        .unwrap();
    let mut github = FakeGitHub(0);
    let board = store
        .assignment_board(&mut github, &policy(), &[first.clone(), second.clone()], 10)
        .unwrap();
    assert_eq!(board.candidates(&first.id).unwrap(), [3, 2, 1]);
    assert_eq!(board.candidates(&second.id).unwrap(), [3, 1]);
    assert!(board.candidates("unknown").is_err());
    assert_eq!(github.0, 1);
    let disabled = store
        .assignment_board(&mut github, &Policy::default(), &[first], 10)
        .unwrap();
    assert!(disabled.board.entries.is_empty());
    assert!(disabled.assignments.is_empty());
    assert_eq!(github.0, 1);
}

struct Intake(Snapshot, usize);
impl GitHub for Intake {
    fn snapshot(&mut self) -> oxa_work::Result<Snapshot> {
        self.1 += 1;
        Ok(self.0.clone())
    }
}

#[test]
fn typed_issue_intake_uses_one_snapshot_and_builder_authority() {
    let fixture = Fixture::new();
    let worker = worker("slot-1");
    let mut source = Intake(
        Snapshot {
            pull_requests: FakeGitHub(0).open_pull_requests().unwrap(),
            issues: (4..=6)
                .map(|number| Issue {
                    number,
                    revision: "source-revision".into(),
                    release_pinned: true,
                    approved: number != 5,
                    has_open_pull_request: number == 6,
                })
                .collect(),
        },
        0,
    );
    let store = fixture.store();
    let disabled = store
        .assignment_board(&mut source, &Policy::default(), from_ref(&worker), 10)
        .unwrap();
    assert!(disabled.board.entries.is_empty());
    assert_eq!(source.1, 0);
    let board = store
        .assignment_board(&mut source, &policy(), from_ref(&worker), 10)
        .unwrap();
    assert_eq!(source.1, 1);
    assert_eq!(board.candidates(&worker.id).unwrap(), [3, 4, 1, 2]);
    let issue = board.board.entries.iter().find(|e| e.number == 4).unwrap();
    assert_eq!(issue.action, Action::BuildIssue);
    assert_eq!(issue.head, "source-revision");
    assert!(store
        .claim_for(issue, &policy(), &worker, 10, 10)
        .unwrap()
        .is_some());
    for number in [5, 6] {
        let waiting = board.board.entries.iter().find(|e| e.number == number).unwrap();
        assert_eq!(waiting.action, Action::WaitIssue);
        assert!(!waiting.eligible);
        assert!(store
            .claim_for(waiting, &policy(), &worker, 10, 10)
            .unwrap()
            .is_none());
    }
    source.0.issues[0].number = 3;
    assert!(Board::read(&mut source, &policy(), &BTreeMap::new(), 10).is_err());
}

#[test]
fn import_resumes_without_resetting_attempts_or_admitting_preserved_jobs() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let worker = worker("slot-1");
    let incoming: BTreeMap<_, _> = [
        (
            3,
            ImportState {
                state: AttemptState {
                    utc_day: 0,
                    attempts: 1,
                    lease: Some(Lease {
                        holder: worker.id.clone(),
                        claimed_at: Some(10),
                        expires_at: 20,
                        generation: (0, 1),
                    }),
                },
                excluded: false,
            },
        ),
        (
            4,
            ImportState {
                state: AttemptState {
                    attempts: 2,
                    ..Default::default()
                },
                excluded: true,
            },
        ),
    ]
    .into();
    assert!(store.import_snapshot(None, &incoming, 11).is_err());
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
    let first = [(3, incoming[&3].clone())].into();
    store
        .import_snapshot(Some(ManualAuthority::Owner), &first, 11)
        .unwrap();
    store
        .import_snapshot(Some(ManualAuthority::Owner), &incoming, 11)
        .unwrap();
    store
        .import_snapshot(Some(ManualAuthority::Owner), &incoming, 11)
        .unwrap();
    let rows = store.assignments().unwrap();
    for number in [3, 4] {
        assert_eq!(rows[&number].state, incoming[&number].state);
        assert_eq!(rows[&number].data.revision, 1);
        assert!(rows[&number].data.import_hold);
    }
    let mut issue = entry();
    issue.number = 4;
    issue.action = Action::BuildIssue;
    assert!(store
        .claim_for(&issue, &policy(), &worker, 21, 10)
        .unwrap()
        .is_none());
    assert!(store
        .claim(&entry(), &policy(), &worker.id, 21, 10)
        .unwrap()
        .is_none());
    let board = store
        .assignment_board(&mut FakeGitHub(0), &policy(), from_ref(&worker), 21)
        .unwrap();
    assert!(!board.candidates(&worker.id).unwrap().contains(&3));
    assert!(store.clear_import_hold(4, None, 1).is_err());
    assert!(store
        .clear_import_hold(4, Some(ManualAuthority::Lead), 0)
        .is_err());
    store
        .clear_import_hold(4, Some(ManualAuthority::Lead), 1)
        .unwrap();
    assert!(store
        .claim_for(&issue, &policy(), &worker, 21, 10)
        .unwrap()
        .is_some());
    assert!(store
        .import_snapshot(Some(ManualAuthority::Owner), &incoming, 21)
        .is_err());
    assert_eq!(store.snapshot().unwrap()[&4].attempts, 3);
}

#[test]
fn invalid_or_conflicting_import_is_preflighted_before_any_state_write() {
    let fixture = Fixture::new();
    let store = fixture.store();
    store
        .claim(&entry(), &policy(), "codex:node/slot-1", 10, 10)
        .unwrap();
    let mut incoming: BTreeMap<_, _> = [2, 3]
        .map(|number| {
            (
                number,
                ImportState {
                    state: AttemptState::default(),
                    excluded: true,
                },
            )
        })
        .into();
    assert!(store
        .import_snapshot(Some(ManualAuthority::Owner), &incoming, 11)
        .is_err());
    assert!(!fixture.0.join("2.board-claim.json").exists());
    incoming.remove(&3);
    incoming.get_mut(&2).unwrap().state.utc_day = 1;
    assert!(store
        .import_snapshot(Some(ManualAuthority::Owner), &incoming, 11)
        .is_err());
    assert!(!fixture.0.join("2.board-claim.json").exists());
    assert_eq!(store.snapshot().unwrap()[&3].attempts, 1);
}
