use super::*;
use crate::board::claims::activity::tests::{fixture, reserve};

fn wait() -> DependencyWait {
    DependencyWait {
        dependency: Dependency::WorkItem(3),
        deadline: 40,
    }
}

fn park(
    claims: &Claims,
    binding: &NativeBinding,
    receipt: &ItemLease,
    wait: &DependencyWait,
    proof: Result<bool>,
) -> Result<bool> {
    let revision = claims.assignments()?[&binding.number].data.revision;
    claims.wait_for_dependency("worker-a", "host-a", binding, receipt, 25, revision, wait, |_| {
        proof
    })
}

#[test]
fn dependency_wait_survives_reopening_and_deadline_is_not_release_or_progress() {
    let (dir, claims, receipt, binding) = fixture();
    let wait = wait();
    assert!(
        serde_json::to_value(claims.assignments().unwrap()[&1].data.clone())
            .unwrap()
            .get("dependency")
            .is_none()
    );
    assert!(!park(&claims, &binding, &receipt, &wait, Ok(false)).unwrap());
    assert!(park(&claims, &binding, &receipt, &wait, Ok(true)).unwrap());
    let claims = Claims::open(dir.path()).unwrap();
    let revision = claims.assignments().unwrap()[&1].data.revision;
    assert!(!park(&claims, &binding, &receipt, &wait, Ok(true)).unwrap());
    assert_eq!(claims.assignments().unwrap()[&1].data.revision, revision);
    assert!(claims
        .observe_item_for(
            "worker-a",
            "host-a",
            &binding,
            &receipt,
            30,
            LeaseSignal::Heartbeat
        )
        .unwrap());
    let row = claims.assignments().unwrap().remove(&1).unwrap();
    assert_eq!(row.data.dependency, Some(wait));
    assert!(!row.stalled(39, Some(5)).unwrap());
    assert!(row.stalled(40, Some(5)).unwrap());
    assert!(row.stalled(100, Some(5)).unwrap());
    assert_eq!(row.state.lease, Some(receipt.lease));
    assert_eq!(row.state.attempts, 1);
    assert!(row.data.native_binding == Some(binding));
}

#[test]
fn invalid_unknown_and_stale_waits_cannot_change_the_current_assignment() {
    let (_dir, claims, receipt, binding) = fixture();
    let wait = wait();
    let revision = claims.assignments().unwrap()[&1].data.revision;
    assert!(park(&claims, &binding, &receipt, &wait, Err("unavailable".into())).is_err());
    for dependency in [
        Dependency::RequiredCi(0),
        Dependency::WorkItem(1),
        Dependency::ReviewedHead {
            number: 2,
            head: "short".into(),
        },
        Dependency::OwnerRead("free text".into()),
    ] {
        let invalid = DependencyWait {
            dependency,
            deadline: 40,
        };
        assert!(park(&claims, &binding, &receipt, &invalid, Ok(true)).is_err());
    }
    let expired = DependencyWait {
        deadline: 25,
        ..wait.clone()
    };
    assert!(park(&claims, &binding, &receipt, &expired, Ok(true)).is_err());
    assert!(claims
        .wait_for_dependency(
            "worker-a",
            "host-a",
            &binding,
            &receipt,
            25,
            0,
            &wait,
            |_| panic!("stale revision")
        )
        .is_err());
    assert_eq!(claims.assignments().unwrap()[&1].data.revision, revision);
    assert!(claims.assignments().unwrap()[&1].data.dependency.is_none());
}

#[test]
fn wait_uses_latest_full_receipt_and_new_admission_clears_its_old_generation() {
    let (_dir, claims, receipt, binding) = fixture();
    let wait = wait();
    assert!(park(&claims, &binding, &receipt, &wait, Ok(true)).unwrap());
    let renewed = claims
        .renew_item_for(1, "worker-a", "host-a", &receipt, 30, 200)
        .unwrap()
        .unwrap();
    let next = DependencyWait {
        dependency: Dependency::OwnerRead("read-1".into()),
        deadline: 50,
    };
    assert!(!park(&claims, &binding, &receipt, &next, Ok(true)).unwrap());
    let revision = claims.assignments().unwrap()[&1].data.revision;
    assert!(claims
        .wait_for_dependency(
            "worker-a",
            "host-b",
            &binding,
            &renewed,
            31,
            revision,
            &next,
            |_| Ok(true)
        )
        .is_err());
    assert!(claims
        .wait_for_dependency(
            "worker-a",
            "host-a",
            &binding,
            &renewed,
            31,
            revision,
            &next,
            |_| Ok(true)
        )
        .unwrap());
    claims
        .release_for(1, "worker-a", &receipt.lease, 32, "finished")
        .unwrap();
    assert!(claims.assignments().unwrap()[&1].data.dependency.is_some());
    reserve(&claims, 33);
    assert!(claims.assignments().unwrap()[&1].data.dependency.is_none());
    assert!(!park(&claims, &binding, &renewed, &next, Ok(true)).unwrap());
}
