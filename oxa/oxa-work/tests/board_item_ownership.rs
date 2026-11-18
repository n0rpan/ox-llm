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

#[test]
fn another_host_cannot_replace_expired_work_and_refusal_names_the_owner() {
    let dir = PrivateDir::new("item-ownership");
    let store = Claims::open(dir.path()).unwrap();
    let first = store
        .claim_item_for(
            &entry(),
            &policy(),
            &worker("worker-a"),
            "host-a",
            (10, 20),
            |_| Ok(None),
        )
        .unwrap()
        .unwrap();
    let reopened = Claims::open(dir.path()).unwrap();
    for now in [11, 31] {
        let denied = reopened.claim_item_for(
            &entry(),
            &policy(),
            &worker("worker-b"),
            "host-b",
            (now, 20),
            |_| Ok(None),
        );
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
