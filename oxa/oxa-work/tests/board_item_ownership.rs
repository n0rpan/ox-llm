#[path = "support/private_dir.rs"]
mod test_dir;
use oxa_work::board::{assignment::*, *};
use test_dir::PrivateDir;

fn worker(id: &str) -> Worker {
    Worker {
        id: id.into(),
        kind: WorkerKind::Codex,
        skills: Default::default(),
        roles: [Role::Builder].into(),
    }
}
fn entry() -> Entry {
    Entry {
        number: 1,
        head: "synthetic-source".into(),
        release_pinned: false,
        action: Action::BuildIssue,
        lease: None,
        attempts_today: 0,
        eligible: true,
    }
}
fn policy() -> Policy {
    Policy {
        enabled: true,
        ..Policy::default()
    }
}
fn claim(store: &Claims, id: &str, host: &str, now: u64) -> ItemLease {
    store
        .claim_item_for(&entry(), &policy(), &worker(id), host, (now, 20), |_| Ok(None))
        .unwrap()
        .unwrap()
}

#[test]
fn another_host_cannot_replace_expired_work_and_refusal_names_the_owner() {
    let dir = PrivateDir::new("item-ownership");
    let store = Claims::open(dir.path()).unwrap();
    let first = claim(&store, "worker-a", "host-a", 10);
    let reopened = Claims::open(dir.path()).unwrap();
    for (now, id) in [(11, "worker-a"), (11, "worker-b"), (31, "worker-b")] {
        let denied = reopened.claim_item_for(&entry(), &policy(), &worker(id), "host-b", (now, 20), |_| {
            Ok(None)
        });
        assert!(
            denied.is_err_and(|reason| reason.contains(&first.lease.holder)),
            "must name the existing owner, even after expiry"
        );
    }
}

#[test]
fn existing_pr_or_branch_refuses_before_charging_an_attempt() {
    let dir = PrivateDir::new("item-existing-work");
    let store = Claims::open(dir.path()).unwrap();
    for existing in [
        ExistingWork::PullRequest {
            number: 2,
            owner: "worker-a".into(),
        },
        ExistingWork::Branch {
            name: "agent/existing".into(),
            owner: "worker-a".into(),
        },
    ] {
        let denied = store.claim_item_for(
            &entry(),
            &policy(),
            &worker("worker-b"),
            "host-b",
            (10, 20),
            |number| {
                assert_eq!(number, 1);
                Ok(Some(existing))
            },
        );
        assert!(
            denied.is_err_and(|reason| reason.contains("worker-a")),
            "existing work must block admission"
        );
        assert!(store.snapshot().unwrap()[&1].lease.is_none());
    }
}

#[test]
fn renewal_and_verified_reclaim_keep_original_binding_and_fence_stale_receipts() {
    let dir = PrivateDir::new("item-renew-reclaim");
    let store = Claims::open(dir.path()).unwrap();
    let first = claim(&store, "worker-a", "host-a", 10);
    let binding = NativeBinding::new(1, &"a".repeat(64), &first.lease, "agent/item").unwrap();
    assert!(store.bind_native("worker-a", &binding).unwrap());
    assert!(store
        .renew_item_for(1, "worker-a", "host-b", &first, 20, 30)
        .is_err());
    let renewed = store
        .renew_item_for(1, "worker-a", "host-a", &first, 20, 30)
        .unwrap()
        .unwrap();
    assert_eq!(renewed.lease, first.lease);
    assert_eq!(renewed.expires_at, 50);
    assert!(store
        .renew_item_for(1, "worker-a", "host-a", &first, 21, 40)
        .unwrap()
        .is_none());
    let reopened = Claims::open(dir.path()).unwrap();
    assert_eq!(reopened.assignments().unwrap()[&1].assignee(51), Some("worker-a"));
    let revision = reopened.assignments().unwrap()[&1].data.revision;
    assert!(reopened
        .unassign(1, Some(ManualAuthority::Lead), revision, 51, "reassign")
        .is_err());
    assert_eq!(
        reopened
            .with_live_native_lease(&binding, || 40, |_| true)
            .unwrap(),
        Some(true)
    );
    assert!(
        reopened
            .native_binding(&binding.native_task_id, &binding.branch)
            .unwrap()
            == Some(binding.clone())
    );
    assert!(!reopened
        .reclaim_item(1, Some(ManualAuthority::Lead), &first, 31, |_| panic!(
            "stale receipt"
        ))
        .unwrap());
    assert!(!reopened
        .reclaim_item(1, Some(ManualAuthority::Lead), &renewed, 49, |_| panic!(
            "still live"
        ))
        .unwrap());
    assert!(reopened
        .reclaim_item(1, None, &renewed, 50, |_| panic!("unauthenticated"))
        .is_err());
    assert!(reopened
        .renew_item_for(1, "worker-a", "host-a", &renewed, 50, 30)
        .unwrap()
        .is_none());
    assert!(!reopened
        .reclaim_item(1, Some(ManualAuthority::Lead), &renewed, 50, |_| Ok(false))
        .unwrap());
    assert!(reopened
        .reclaim_item(1, Some(ManualAuthority::Lead), &renewed, 50, |_| Err(
            "host unavailable".into()
        ))
        .is_err());
    assert!(reopened
        .claim_for(&entry(), &policy(), &worker("worker-b"), 51, 20)
        .is_err());
    assert!(reopened
        .reclaim_item(1, Some(ManualAuthority::Lead), &renewed, 51, |receipt| {
            assert_eq!(receipt, &renewed);
            Ok(true)
        })
        .unwrap());
    assert!(reopened
        .claim_for(&entry(), &policy(), &worker("worker-b"), 52, 20)
        .is_err());
    let next = claim(&reopened, "worker-b", "host-b", 52);
    assert_ne!(next.lease.generation, first.lease.generation);
    assert!(!reopened
        .release_for(1, "worker-a", &first.lease, 53, "stale")
        .unwrap());
    assert_eq!(reopened.assignments().unwrap()[&1].state.attempts, 2);
    assert_eq!(
        reopened
            .with_live_native_lease(&binding, || 53, |_| true)
            .unwrap(),
        None
    );
}

#[test]
fn default_off_never_observes_and_incomplete_evidence_fails_closed() {
    let dir = PrivateDir::new("item-off");
    let store = Claims::open(dir.path()).unwrap();
    assert!(store
        .claim_item_for(
            &entry(),
            &Policy::default(),
            &worker("worker-a"),
            "host-a",
            (10, 20),
            |_| panic!("disabled")
        )
        .unwrap()
        .is_none());
    assert!(store.snapshot().unwrap().is_empty());
    assert!(store
        .claim_item_for(
            &entry(),
            &policy(),
            &worker("worker-a"),
            "host-a",
            (10, 20),
            |_| Err("incomplete snapshot".into())
        )
        .is_err());
    assert_eq!(store.snapshot().unwrap()[&1].attempts, 0);
}
