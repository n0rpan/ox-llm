#[path = "support/private_dir.rs"]
mod test_dir;
use oxa_work::board::{assignment::*, *};
use std::{collections::BTreeMap, fs};
use test_dir::PrivateDir;

fn setup() -> (PrivateDir, Claims, Lease) {
    let dir = PrivateDir::new("native-binding");
    let store = Claims::open(dir.path()).unwrap();
    store
        .import_snapshot(
            Some(ManualAuthority::Owner),
            &BTreeMap::from([(
                1,
                ImportState {
                    state: AttemptState::default(),
                    excluded: false,
                },
            )]),
            10,
        )
        .unwrap();
    let lease = store
        .claim_idle_for(&entry(), &policy(), &worker(), 10, 20)
        .unwrap()
        .unwrap();
    (dir, store, lease)
}
fn worker() -> Worker {
    Worker {
        id: "codex:node/slot-1".into(),
        kind: WorkerKind::Codex,
        skills: Default::default(),
        roles: [Role::Builder].into(),
    }
}
fn policy() -> Policy {
    Policy {
        enabled: true,
        ..Policy::default()
    }
}
fn entry() -> Entry {
    Entry {
        number: 1,
        head: "synthetic-head".into(),
        release_pinned: false,
        action: Action::FixFindings,
        lease: None,
        attempts_today: 0,
        eligible: true,
    }
}
fn binding(lease: &Lease) -> NativeBinding {
    NativeBinding::new(1, &"a".repeat(64), lease, "agent/test").unwrap()
}

#[test]
fn binding_is_durable_idempotent_and_immutable_for_the_original_lease() {
    let (dir, store, lease) = setup();
    let binding = binding(&lease);
    assert_eq!(binding.native_task_id, format!("board-{}-0-1", "a".repeat(64)));
    assert!(store.bind_native(&worker().id, &binding).unwrap());
    let path = dir.path().join("1.board-claim.json");
    let bytes = fs::read(&path).unwrap();
    let reopened = Claims::open(dir.path()).unwrap();
    assert!(reopened.bind_native(&worker().id, &binding).unwrap());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    let mut changed = binding.clone();
    changed.branch = "agent/other".into();
    assert!(!reopened.bind_native(&worker().id, &changed).unwrap());
    changed = NativeBinding::new(1, &"b".repeat(64), &lease, &binding.branch).unwrap();
    assert!(!reopened.bind_native(&worker().id, &changed).unwrap());
    assert!(
        reopened
            .native_binding(&binding.native_task_id, &binding.branch)
            .unwrap()
            == Some(binding)
    );
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn every_original_lease_component_and_holder_authentication_are_fenced() {
    let (dir, store, lease) = setup();
    let before = fs::read(dir.path().join("1.board-claim.json")).unwrap();
    assert!(store.bind_native("codex:node/other", &binding(&lease)).is_err());
    for field in 0..5 {
        let mut stale = lease.clone();
        match field {
            0 => stale.holder = "codex:node/other".into(),
            1 => stale.expires_at += 1,
            2 => stale.claimed_at = Some(11),
            3 => stale.generation.0 += 1,
            _ => stale.generation.1 += 1,
        }
        assert!(!store.bind_native(&stale.holder, &binding(&stale)).unwrap());
    }
    assert_eq!(fs::read(dir.path().join("1.board-claim.json")).unwrap(), before);
}

#[test]
fn completion_lookup_never_substitutes_a_new_lease_or_a_different_branch() {
    let (_dir, store, lease) = setup();
    let original = binding(&lease);
    assert!(store.bind_native(&worker().id, &original).unwrap());
    assert!(store
        .native_binding(&original.native_task_id, "agent/wrong")
        .unwrap()
        .is_none());
    assert!(store
        .native_binding("wrong-task", &original.branch)
        .unwrap()
        .is_none());
    assert!(store
        .release_for(1, &worker().id, &lease, 20, "finished")
        .unwrap());
    assert!(
        store
            .native_binding(&original.native_task_id, &original.branch)
            .unwrap()
            == Some(original.clone())
    );
    let replacement = store
        .claim_for(&entry(), &policy(), &worker(), 21, 20)
        .unwrap()
        .unwrap();
    assert!(store
        .native_binding(&original.native_task_id, &original.branch)
        .unwrap()
        .is_none());
    assert!(!store.bind_native(&worker().id, &original).unwrap());
    assert!(!store
        .release_for(1, &worker().id, &lease, 22, "stale completion")
        .unwrap());
    assert!(store.bind_native(&worker().id, &binding(&replacement)).unwrap());
    assert_eq!(store.snapshot().unwrap()[&1].attempts, 2);
}

#[test]
fn old_rows_remain_readable_and_malformed_saved_bindings_fail_closed() {
    let (dir, store, lease) = setup();
    let path = dir.path().join("1.board-claim.json");
    let old: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(old["data"].get("native_binding").is_none());
    assert!(store.assignments().unwrap()[&1].data.native_binding.is_none());
    assert!(store.bind_native(&worker().id, &binding(&lease)).unwrap());
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for field in ["number", "key", "native_task_id", "branch", "lease"] {
        let mut corrupt = saved.clone();
        corrupt["data"]["native_binding"][field] = serde_json::json!("invalid");
        fs::write(&path, serde_json::to_vec(&corrupt).unwrap()).unwrap();
        assert!(Claims::open(dir.path()).unwrap().assignments().is_err());
    }
}

#[test]
fn constructor_rejects_invalid_identity_without_touching_the_store() {
    let (_dir, _store, lease) = setup();
    for branch in ["", "-bad", "a..b", "a//b", "a.lock", "a\nb", "a/.bad", "a@{b"] {
        assert!(NativeBinding::new(1, &"a".repeat(64), &lease, branch).is_err());
    }
    assert!(NativeBinding::new(0, &"a".repeat(64), &lease, "agent/test").is_err());
    assert!(NativeBinding::new(1, &"A".repeat(64), &lease, "agent/test").is_err());
    assert!(NativeBinding::new(1, "short", &lease, "agent/test").is_err());
}

#[test]
fn concurrent_conflicting_bindings_have_one_durable_winner() {
    let (_dir, store, lease) = setup();
    let first = binding(&lease);
    let second = NativeBinding::new(1, &"b".repeat(64), &lease, "agent/other").unwrap();
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            store.bind_native(&worker().id, &first).unwrap()
        });
        let b = scope.spawn(|| {
            barrier.wait();
            store.bind_native(&worker().id, &second).unwrap()
        });
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_ne!(results.0, results.1);
    let winner = store
        .assignments()
        .unwrap()
        .remove(&1)
        .unwrap()
        .data
        .native_binding
        .unwrap();
    assert!(winner == if results.0 { first } else { second });
    assert_eq!(store.snapshot().unwrap()[&1].attempts, 1);
}

#[test]
fn a_live_or_expired_binding_cannot_be_reused_for_another_item() {
    let (_dir, store, lease) = setup();
    let original = binding(&lease);
    assert!(store.bind_native(&worker().id, &original).unwrap());
    let mut other_entry = entry();
    other_entry.number = 2;
    // Generic claim_for remains available, including after an existing lease expires.
    let other_lease = store
        .claim_for(&other_entry, &policy(), &worker(), 10, 20)
        .unwrap()
        .unwrap();
    let same_task = NativeBinding::new(2, &original.key, &other_lease, "agent/other").unwrap();
    assert!(!store.bind_native(&worker().id, &same_task).unwrap());
    let same_branch = NativeBinding::new(2, &"b".repeat(64), &other_lease, &original.branch).unwrap();
    assert!(!store.bind_native(&worker().id, &same_branch).unwrap());
    assert!(store
        .release_for(1, &worker().id, &lease, 40, "reconciled")
        .unwrap());
    assert!(store.bind_native(&worker().id, &same_branch).unwrap());
    assert!(!store.bind_native(&worker().id, &original).unwrap());
}
