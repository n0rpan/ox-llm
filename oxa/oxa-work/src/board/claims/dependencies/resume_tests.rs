use super::*;
use crate::board::assignment::{Control, ManualAuthority, Role, Worker, WorkerKind};
use crate::board::claims::activity::tests::fixture;
use crate::board::ImportState;

fn wait() -> DependencyWait {
    DependencyWait {
        dependency: Dependency::WorkItem(3),
        deadline: 40,
    }
}

fn park(claims: &Claims, binding: &NativeBinding, receipt: &ItemLease, now: u64, wait: &DependencyWait) {
    let revision = claims.assignments().unwrap()[&binding.number].data.revision;
    assert!(claims
        .wait_for_dependency(
            "worker-a",
            "host-a",
            binding,
            receipt,
            now,
            revision,
            wait,
            |_| Ok(true)
        )
        .unwrap());
}

fn resume(
    claims: &Claims,
    binding: &NativeBinding,
    receipt: &ItemLease,
    now: u64,
    wait: &DependencyWait,
    proof: bool,
) -> bool {
    let revision = claims.assignments().unwrap()[&binding.number].data.revision;
    claims
        .resume_dependency_for(
            "worker-a",
            "host-a",
            binding,
            receipt,
            now,
            revision,
            wait,
            |_| Ok(proof),
        )
        .unwrap()
}

fn pick(claims: &Claims, now: u64) -> Option<ItemLease> {
    let entry = Entry {
        number: 2,
        head: "synthetic-source".into(),
        release_pinned: false,
        action: Action::BuildIssue,
        lease: None,
        attempts_today: 0,
        eligible: true,
    };
    let worker = Worker {
        id: "worker-a".into(),
        kind: WorkerKind::Codex,
        skills: Default::default(),
        roles: [Role::Builder].into(),
    };
    claims
        .claim_idle_item_for(
            &entry,
            &Policy {
                enabled: true,
                ..Policy::default()
            },
            &worker,
            "host-a",
            (now, 100),
            |_, _| Ok(None),
        )
        .unwrap()
}

#[test]
fn parked_worker_takes_next_item_and_only_fresh_clearance_resumes_original_attempt() {
    let (dir, claims, receipt, binding) = fixture();
    claims
        .control(2, Some(ManualAuthority::Lead), 0, Control::OwnerOnly(false))
        .unwrap();
    assert!(pick(&claims, 11).is_none());
    let wait = wait();
    park(&claims, &binding, &receipt, 12, &wait);
    let claims = Claims::open(dir.path()).unwrap();
    let other = pick(&claims, 20).expect("verified parking permits actual next-item admission");
    assert!(!resume(&claims, &binding, &receipt, 22, &wait, true));
    claims
        .release_for(2, "worker-a", &other.lease, 23, "finished")
        .unwrap();
    assert!(!resume(&claims, &binding, &receipt, 41, &wait, false));
    let wrong = DependencyWait {
        deadline: 50,
        ..wait.clone()
    };
    assert!(!resume(&claims, &binding, &receipt, 41, &wrong, true));
    assert!(resume(&claims, &binding, &receipt, 42, &wait, true));
    assert!(pick(&claims, 43).is_none());
    let row = claims.assignments().unwrap().remove(&1).unwrap();
    assert_eq!(row.state.lease, Some(receipt.lease));
    assert_eq!(row.state.attempts, 1);
    assert!(row.data.native_binding == Some(binding));
    assert!(row.data.dependency.is_none());
}

#[test]
fn concurrent_clearance_cannot_resume_two_running_items_for_the_same_worker() {
    let (_dir, claims, receipt, binding) = fixture();
    let wait = wait();
    park(&claims, &binding, &receipt, 12, &wait);
    claims
        .control(2, Some(ManualAuthority::Lead), 0, Control::OwnerOnly(false))
        .unwrap();
    let other = pick(&claims, 20).unwrap();
    let other_binding = NativeBinding::new(2, &"b".repeat(64), &other.lease, "agent/other").unwrap();
    assert!(claims.bind_native("worker-a", &other_binding).unwrap());
    park(&claims, &other_binding, &other, 21, &wait);
    let outcomes = std::thread::scope(|scope| {
        let first = scope.spawn(|| resume(&claims, &binding, &receipt, 30, &wait, true));
        let second = scope.spawn(|| resume(&claims, &other_binding, &other, 30, &wait, true));
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(outcomes.into_iter().filter(|running| *running).count(), 1);
    let rows = claims.assignments().unwrap();
    assert_eq!(
        rows.values().filter(|row| row.data.dependency.is_some()).count(),
        1
    );
    assert!(rows
        .values()
        .all(|row| row.state.lease.is_some() && row.state.attempts == 1));
}

#[test]
fn imported_holds_and_stale_observations_still_block_yield_and_resume() {
    let (_dir, claims, receipt, binding) = fixture();
    let wait = wait();
    park(&claims, &binding, &receipt, 12, &wait);
    let old_revision = claims.assignments().unwrap()[&1].data.revision;
    let state = claims.snapshot().unwrap().remove(&1).unwrap();
    claims
        .import_snapshot(
            Some(ManualAuthority::Lead),
            &BTreeMap::from([(
                1,
                ImportState {
                    state,
                    excluded: true,
                },
            )]),
            13,
        )
        .unwrap();
    claims
        .control(2, Some(ManualAuthority::Lead), 0, Control::OwnerOnly(false))
        .unwrap();
    assert!(pick(&claims, 20).is_none());
    assert!(claims
        .resume_dependency_for(
            "worker-a",
            "host-a",
            &binding,
            &receipt,
            20,
            old_revision,
            &wait,
            |_| panic!("stale revision")
        )
        .is_err());
    let revision = claims.assignments().unwrap()[&1].data.revision;
    assert!(!claims
        .resume_dependency_for(
            "worker-a",
            "host-a",
            &binding,
            &receipt,
            20,
            revision,
            &wait,
            |_| panic!("import hold is not cleared by a dependency")
        )
        .unwrap());
    assert!(claims.assignments().unwrap()[&1].state.lease.is_some());
}
