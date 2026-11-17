use oxa_work::board::assignment::{Control, ManualAuthority, Pin, Role, Worker, WorkerKind};
use oxa_work::board::*;
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
    let witness = fixture.0.join("3.board-claim.created");
    fs::remove_file(&witness).unwrap();
    assert!(fixture
        .store()
        .control(3, Some(ManualAuthority::Owner), 0, Control::OwnerOnly(false))
        .is_err());
    assert!(witness.exists());
    let rows = fixture.store().assignments().unwrap();
    assert_eq!(rows[&3].data.revision, 1);
    assert!(rows[&3].data.policy.as_ref().unwrap().owner_only);
    assert_eq!(rows[&3].state.attempts, 0);
    assert_eq!(rows[&3].assignee(10), None);
    fs::remove_file(fixture.0.join("3.board-claim.json")).unwrap();
    fs::remove_file(fixture.0.join("3.board-claim.lock")).unwrap();
    assert!(fixture.store().assignments().is_err());
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
