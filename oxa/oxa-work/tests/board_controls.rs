#[path = "support/private_dir.rs"]
mod test_dir;
use oxa_work::board::assignment::{Control, ManualAuthority, Pin, Role, Worker, WorkerKind};
use oxa_work::board::*;
use std::slice::from_ref;
use std::{collections::BTreeMap, fs};
use test_dir::PrivateDir;

struct Fixture(PrivateDir);
impl Fixture {
    fn new() -> Self {
        Self(PrivateDir::new("board-controls"))
    }
    fn store(&self) -> Claims {
        Claims::open(self.0.path()).unwrap()
    }
    fn establish(&self, numbers: impl IntoIterator<Item = u64>) {
        let incoming = numbers
            .into_iter()
            .map(|number| {
                (
                    number,
                    ImportState {
                        state: AttemptState::default(),
                        excluded: false,
                    },
                )
            })
            .collect();
        self.store()
            .import_snapshot(Some(ManualAuthority::Owner), &incoming, 10)
            .unwrap();
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
    assert_eq!(fs::read_dir(fixture.0.path()).unwrap().count(), 0);
    fixture
        .store()
        .control(3, Some(ManualAuthority::Lead), 0, Control::OwnerOnly(true))
        .unwrap();
    let witness = fixture.0.path().join("3.board-claim.created");
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
    fs::remove_file(fixture.0.path().join("3.board-claim.json")).unwrap();
    fs::remove_file(fixture.0.path().join("3.board-claim.lock")).unwrap();
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
    assert!(store.release_for(3, &worker.id, &lease, 20, "completed").unwrap());
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
    let mut worker = worker("slot-1");
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
            incomplete: vec![],
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
    worker.roles = [Role::Reviewer].into();
    let denied = store.claim_for(issue, &policy(), &worker, 10, 10).unwrap();
    assert!(denied.is_none());
    assert!(store
        .claim_idle_for(issue, &policy(), &worker, 10, 10)
        .unwrap()
        .is_none());
    let legacy = store.claim(issue, &policy(), &worker.id, 10, 10).unwrap();
    assert!(legacy.is_none());
    worker.roles = [Role::Builder].into();
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
    assert_eq!(fs::read_dir(fixture.0.path()).unwrap().count(), 0);
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
        .claim_idle_for(&issue, &policy(), &worker, 21, 10)
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
    assert!(!fixture.0.path().join("2.board-claim.json").exists());
    incoming.remove(&3);
    incoming.get_mut(&2).unwrap().state.utc_day = 1;
    assert!(store
        .import_snapshot(Some(ManualAuthority::Owner), &incoming, 11)
        .is_err());
    assert!(!fixture.0.path().join("2.board-claim.json").exists());
    assert_eq!(store.snapshot().unwrap()[&3].attempts, 1);
}

#[test]
fn manual_assignment_and_unassignment_share_revision_fencing_and_attempts() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let first = worker("slot-1");
    let second = worker("slot-2");
    let entry = entry();
    assert!(store.assign(&entry, &policy(), None, 0, &first, 10, 10).is_err());
    assert_eq!(fs::read_dir(fixture.0.path()).unwrap().count(), 0);
    assert!(store
        .assign(
            &entry,
            &Policy::default(),
            Some(ManualAuthority::Owner),
            0,
            &first,
            10,
            10
        )
        .unwrap()
        .is_none());
    assert_eq!(fs::read_dir(fixture.0.path()).unwrap().count(), 0);
    let lease = store
        .assign(&entry, &policy(), Some(ManualAuthority::Lead), 0, &first, 10, 10)
        .unwrap()
        .unwrap();
    assert!(store
        .assign(
            &entry,
            &policy(),
            Some(ManualAuthority::Owner),
            1,
            &second,
            11,
            10
        )
        .unwrap()
        .is_none());
    assert!(store.unassign(3, None, 1, 11, "reassign").is_err());
    assert!(store
        .unassign(3, Some(ManualAuthority::Lead), 0, 11, "reassign")
        .is_err());
    assert!(store
        .unassign(3, Some(ManualAuthority::Lead), 1, 11, "reassign")
        .unwrap());
    let next = store
        .assign(
            &entry,
            &policy(),
            Some(ManualAuthority::Owner),
            2,
            &second,
            11,
            10,
        )
        .unwrap()
        .unwrap();
    assert_ne!(lease.generation, next.generation);
    assert!(!store
        .release_for(3, &first.id, &lease, 12, "stale completion")
        .unwrap());
    assert!(store
        .release_for(3, &first.id, &next, 12, "wrong worker")
        .is_err());
    assert!(store.release(3, &next).is_err());
    assert!(store
        .release_for(3, &second.id, &next, 12, " completed ")
        .unwrap());
    let rows = fixture.store().assignments().unwrap();
    assert_eq!(rows[&3].data.revision, 4);
    assert_eq!(rows[&3].state.attempts, 2);
    assert_eq!(rows[&3].assignee(12), None);
    let ended = rows[&3].data.last_release.as_ref().unwrap();
    assert_eq!(ended.holder, second.id);
    assert_eq!(ended.released_at, 12);
    assert_eq!(ended.reason, "completed");
}

#[test]
fn manual_assignment_never_bypasses_pins_or_the_three_attempt_cap() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let worker = worker("slot-1");
    let entry = entry();
    store
        .control(
            3,
            Some(ManualAuthority::Owner),
            0,
            Control::Pin(Pin::Worker(worker.id.clone())),
        )
        .unwrap();
    assert!(store
        .assign(
            &entry,
            &policy(),
            Some(ManualAuthority::Owner),
            1,
            &crate::worker("slot-2"),
            10,
            10
        )
        .unwrap()
        .is_none());
    for attempt in 0..3 {
        let revision = 1 + attempt * 2;
        let lease = store
            .assign(
                &entry,
                &policy(),
                Some(ManualAuthority::Owner),
                revision,
                &worker,
                10,
                10,
            )
            .unwrap()
            .unwrap();
        assert_eq!(lease.generation.1, attempt as u32 + 1);
        assert!(store
            .unassign(3, Some(ManualAuthority::Owner), revision + 1, 10, "retry needed")
            .unwrap());
    }
    assert!(store
        .assign(
            &entry,
            &policy(),
            Some(ManualAuthority::Owner),
            7,
            &worker,
            10,
            10
        )
        .unwrap()
        .is_none());
    let rows = store.assignments().unwrap();
    assert_eq!(rows[&3].state.attempts, 3);
    assert_eq!(
        rows[&3].data.policy.as_ref().unwrap().pin,
        Some(Pin::Worker(worker.id))
    );
}

#[test]
fn releases_validate_reasons_and_repair_witnesses_without_refunding() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let worker = worker("slot-1");
    let lease = store
        .claim_for(&entry(), &policy(), &worker, 10, 10)
        .unwrap()
        .unwrap();
    for reason in ["", "line\nline", "control\0text", &"x".repeat(513)] {
        assert!(store.release_for(3, &worker.id, &lease, 11, reason).is_err());
    }
    assert!(store.release_for(3, &worker.id, &lease, 9, "backwards").is_err());
    fs::remove_file(fixture.0.path().join("3.board-claim.created")).unwrap();
    assert!(store.release_for(3, &worker.id, &lease, 11, "finished").unwrap());
    assert_eq!(store.snapshot().unwrap()[&3].attempts, 1);
    fs::remove_file(fixture.0.path().join("3.board-claim.json")).unwrap();
    fs::remove_file(fixture.0.path().join("3.board-claim.lock")).unwrap();
    assert!(store.snapshot().is_err());
}

#[test]
fn records_without_release_metadata_remain_readable() {
    let fixture = Fixture::new();
    let store = fixture.store();
    store
        .claim_for(&entry(), &policy(), &worker("slot-1"), 10, 10)
        .unwrap();
    let path = fixture.0.path().join("3.board-claim.json");
    let mut saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    saved["data"].as_object_mut().unwrap().remove("last_release");
    fs::write(path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let rows = store.assignments().unwrap();
    assert_eq!(rows[&3].state.attempts, 1);
    assert!(rows[&3].data.last_release.is_none());
}

#[test]
fn review_reservations_survive_reopening_and_head_changes() {
    use oxa_work::board::review::{
        self, Change, ChangeKind, Dispatch, Facts, GitHubRequestReceipt, Plan, Request, RequestRemote,
        Reviewer,
    };

    struct ReviewPr(PullRequest);
    impl GitHub for ReviewPr {
        fn open_pull_requests(&mut self) -> oxa_work::Result<Vec<PullRequest>> {
            Ok(vec![self.0.clone()])
        }
    }

    let plan = |head: &str, patch_id: &str| -> Plan {
        let changes = [Change {
            path: "src/main.rs",
            previous_path: None,
            kind: ChangeKind::Source,
            deleted_lines: 0,
            deleted_file: false,
        }];
        let facts = Facts {
            changes: &changes,
            two_review: false,
            required_ci: RequiredCi::Pending,
            draft: false,
            review_admitted: true,
            unresolved_findings: false,
            body: "",
            head,
            patch_id: Some(patch_id),
            main_merge_predecessors: &[],
            owner_approval_predecessors: &[],
            requested: &[],
            approvals: &[],
        };
        let mut pr = FakeGitHub(0).open_pull_requests().unwrap().remove(0);
        pr.head = head.into();
        pr.findings = false;
        let (_, mut plans) = Board::read_with_review_policy(
            &mut ReviewPr(pr),
            &policy(),
            &BTreeMap::new(),
            0,
            &review::Policy {
                enabled: true,
                paths: &[],
            },
            &BTreeMap::from([(1, facts)]),
        )
        .unwrap();
        plans.remove(&1).unwrap()
    };

    let fixture = Fixture::new();
    let claims = fixture.store();
    let patch_id = "a".repeat(40);
    let original = plan("tested-head", &patch_id);
    let request = &original.requests()[0];
    assert_eq!(request.reviewer(), Reviewer::Codex);
    assert!(claims.reserve_review_request(request, None).is_err());
    let owner = Some(ManualAuthority::Owner);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| claims.reserve_review_request(request, owner).unwrap());
        let second = scope.spawn(|| claims.reserve_review_request(request, owner).unwrap());
        assert_ne!(first.join().unwrap(), second.join().unwrap());
    });

    let reopened = fixture.store();
    let merged = plan("main-merge-head", &patch_id);
    assert!(!reopened
        .reserve_review_request(&merged.requests()[0], owner)
        .unwrap());
    let changed = plan("edited-head", &"b".repeat(40));
    assert!(reopened
        .reserve_review_request(&changed.requests()[0], owner)
        .unwrap());
    assert_eq!(reopened.snapshot().unwrap()[&1].attempts, 0);

    #[derive(Default)]
    struct Remote {
        eligible: bool,
        submissions: usize,
    }
    impl RequestRemote for Remote {
        fn current_and_admitted(&mut self, _: &Request) -> oxa_work::Result<bool> {
            Ok(self.eligible)
        }
        fn send_once(&mut self, _: &Request) -> oxa_work::Result<()> {
            self.submissions += 1;
            Err("synthetic connection loss after sending".into())
        }
    }
    let plan = plan(&"c".repeat(40), &"d".repeat(40));
    let request = &plan.requests()[0];
    let marker = request.github_marker().unwrap();
    assert!(GitHubRequestReceipt::from_comment("review-app[bot]", "untrusted", &marker).is_none());
    let receipt = GitHubRequestReceipt::from_comment(
        "review-app[bot]",
        "review-app[bot]",
        &format!("@codex review\n{marker}"),
    )
    .unwrap();
    assert_eq!(receipt.receipt().patch_id, request.patch_id());
    assert_eq!(receipt.receipt().reviewer, Reviewer::Codex);

    let mut remote = Remote::default();
    assert_eq!(
        reopened
            .dispatch_review_request(&mut remote, request, owner)
            .unwrap(),
        Dispatch::Held
    );
    assert_eq!(remote.submissions, 0);
    remote.eligible = true;
    assert!(reopened
        .dispatch_review_request(&mut remote, request, owner)
        .is_err());
    assert_eq!(
        fixture
            .store()
            .dispatch_review_request(&mut remote, request, owner)
            .unwrap(),
        Dispatch::AlreadyReserved
    );
    assert_eq!(remote.submissions, 1);
}

#[test]
fn idle_claims_preserve_expired_work_until_fenced_completion() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let worker = worker("slot-1");
    let first = entry();
    let mut next = first.clone();
    next.number = 2;
    assert!(store
        .claim_idle_for(&next, &Policy::default(), &worker, 10, 10)
        .unwrap()
        .is_none());
    assert_eq!(fs::read_dir(fixture.0.path()).unwrap().count(), 0);
    fixture.establish([1, 2, 6]);
    let lease = store
        .claim_for(&first, &policy(), &worker, 10, 10)
        .unwrap()
        .unwrap();
    // Expiry is not evidence that the existing helper actually finished.
    assert!(store
        .claim_idle_for(&next, &policy(), &worker, 20, 10)
        .unwrap()
        .is_none());
    assert_eq!(store.snapshot().unwrap()[&2].attempts, 0);
    let other = crate::worker("slot-2");
    assert!(store
        .claim_idle_for(&first, &policy(), &other, 20, 10)
        .unwrap()
        .is_none());
    let mut independent = first.clone();
    independent.number = 1;
    assert!(store
        .claim_idle_for(&independent, &policy(), &other, 20, 10)
        .unwrap()
        .is_some());
    assert!(store
        .release_for(3, &worker.id, &lease, 20, "verified completion")
        .unwrap());
    let next_lease = store
        .claim_idle_for(&next, &policy(), &worker, 20, 10)
        .unwrap()
        .unwrap();
    let rows = fixture.store().assignments().unwrap();
    assert_eq!(rows[&3].state.attempts, 1);
    assert_eq!(rows[&2].state.attempts, 1);
    assert_eq!(rows[&2].state.lease, Some(next_lease));
    assert!(rows[&3].state.lease.is_none());
    // Losing another worker's established state must fail the complete idle check closed.
    fs::remove_file(fixture.0.path().join("1.board-claim.json")).unwrap();
    next.number = 6;
    let untouched = fs::read(fixture.0.path().join("6.board-claim.json")).unwrap();
    assert!(store
        .claim_idle_for(&next, &policy(), &crate::worker("slot-3"), 21, 10)
        .is_err());
    assert_eq!(
        fs::read(fixture.0.path().join("6.board-claim.json")).unwrap(),
        untouched
    );
}

#[test]
fn concurrent_idle_reservations_share_one_worker_fence_across_refs() {
    let fixture = Fixture::new();
    fixture.establish(1..=6);
    let barrier = std::sync::Barrier::new(6);
    let winners = std::thread::scope(|scope| {
        let mut pending = vec![];
        for number in 1..=6 {
            let store = fixture.store();
            let worker = worker("shared-slot");
            let mut candidate = entry();
            candidate.number = number;
            let barrier = &barrier;
            pending.push(scope.spawn(move || {
                barrier.wait();
                store
                    .claim_idle_for(&candidate, &policy(), &worker, 10, 10)
                    .unwrap()
            }));
        }
        pending
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(Option::is_some)
            .count()
    });
    assert_eq!(winners, 1);
    let rows = fixture.store().assignments().unwrap();
    assert_eq!(rows.values().map(|row| row.state.attempts).sum::<u32>(), 1);
    assert_eq!(rows.values().filter(|row| row.state.lease.is_some()).count(), 1);
}

#[test]
fn idle_claims_require_saved_state_not_a_denied_claim_marker() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let mut candidate = entry();
    candidate.number = 44;
    candidate.action = Action::BuildIssue;
    let builder = worker("slot-1");
    let reviewer = Worker {
        roles: [Role::Reviewer].into(),
        ..builder.clone()
    };
    assert!(store
        .claim_for(&candidate, &policy(), &reviewer, 10, 10)
        .unwrap()
        .is_none());
    assert!(fixture.0.path().join("44.board-claim.lock").exists());
    assert!(!fixture.0.path().join("44.board-claim.json").exists());
    assert!(store
        .claim_idle_for(&candidate, &policy(), &builder, 10, 10)
        .unwrap()
        .is_none());
    assert!(!fixture.0.path().join("44.board-claim.json").exists());
    fixture.establish([44]);
    assert!(store
        .claim_idle_for(&candidate, &policy(), &reviewer, 10, 10)
        .unwrap()
        .is_none());
    assert_eq!(store.snapshot().unwrap()[&44].attempts, 0);
    assert!(store
        .claim_idle_for(&candidate, &policy(), &builder, 10, 10)
        .unwrap()
        .is_some());
    assert_eq!(fixture.store().snapshot().unwrap()[&44].attempts, 1);
}

#[test]
fn hosted_idle_admission_keeps_bootstrap_and_cross_item_worker_fences() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let worker = worker("hosted-slot");
    let first = entry();
    let mut second = first.clone();
    second.number = 2;
    assert!(store
        .claim_idle_item_for(&first, &policy(), &worker, "host-a", (10, 20), |_, _| panic!(
            "unknown row"
        ))
        .unwrap()
        .is_none());
    fixture.establish([first.number, second.number]);
    let receipt = store
        .claim_idle_item_for(&first, &policy(), &worker, "host-a", (10, 20), |_, _| Ok(None))
        .unwrap()
        .unwrap();
    assert_eq!(
        store.assignments().unwrap()[&first.number]
            .data
            .item_owner
            .as_ref()
            .expect("durable host fence")
            .host_id,
        "host-a"
    );
    for now in [11, 31] {
        assert!(store
            .claim_idle_item_for(&second, &policy(), &worker, "host-b", (now, 20), |_, _| panic!(
                "busy worker"
            ))
            .unwrap()
            .is_none());
    }
    assert!(store
        .release_for(
            first.number,
            &worker.id,
            &receipt.lease,
            32,
            "verified completion"
        )
        .unwrap());
    assert!(store
        .claim_idle_item_for(&second, &policy(), &worker, "host-b", (33, 20), |_, _| Ok(None))
        .unwrap()
        .is_some());
}
