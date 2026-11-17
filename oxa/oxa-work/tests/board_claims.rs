use oxa_work::board::*;
use std::{collections::BTreeMap, fs, os::unix::fs::DirBuilderExt, path::PathBuf, process::Command};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "oxa-board-claims-{}-{}",
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
struct FakeGitHub;
impl GitHub for FakeGitHub {
    fn open_pull_requests(&mut self) -> oxa_work::Result<Vec<PullRequest>> {
        Ok(vec![PullRequest {
            number: 1,
            head: "synthetic-head".into(),
            release_pinned: true,
            findings: true,
            required_ci: RequiredCi::Passed,
            mergeability: Mergeability::Mergeable,
            body: String::new(),
            removes_source: false,
            draft: false,
            reviewed: false,
        }])
    }
}
fn policy() -> Policy {
    Policy {
        enabled: true,
        ..Policy::default()
    }
}
fn entry() -> Entry {
    Board::read(&mut FakeGitHub, &policy(), &BTreeMap::new(), 0)
        .unwrap()
        .entries
        .remove(0)
}

#[test]
fn disabled_and_zero_cap_do_not_create_claim_files() {
    let fixture = Fixture::new();
    let store = fixture.store();
    for policy in [
        Policy::default(),
        Policy {
            enabled: true,
            max_attempts_per_day: 0,
        },
    ] {
        assert!(store
            .claim(&entry(), &policy, "worker-a", 10, 10)
            .unwrap()
            .is_none());
    }
    assert!(store.snapshot().unwrap().is_empty());
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
}

#[test]
fn durable_daily_cap_and_stale_release_fencing() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let first = store
        .claim(&entry(), &policy(), "worker-a", 10, 10)
        .unwrap()
        .unwrap();
    assert!(fixture
        .store()
        .claim(&entry(), &policy(), "worker-b", 19, 10)
        .unwrap()
        .is_none());
    let second = fixture
        .store()
        .claim(&entry(), &policy(), "worker-b", 20, 10)
        .unwrap()
        .unwrap();
    assert!(!store.release(1, &first).unwrap());
    assert!(store.release(1, &second).unwrap());
    let third = store
        .claim(&entry(), &policy(), "worker-c", 21, 10)
        .unwrap()
        .unwrap();
    assert!(store.release(1, &third).unwrap());
    assert!(fixture
        .store()
        .claim(&entry(), &policy(), "worker-d", 22, 10)
        .unwrap()
        .is_none());
    assert_eq!(store.snapshot().unwrap()[&1].attempts, 3);
    assert!(store
        .claim(&entry(), &policy(), "worker-e", 86_400, 10)
        .unwrap()
        .is_some());
    assert_eq!(fixture.store().snapshot().unwrap()[&1].attempts, 1);
}

#[test]
fn midnight_does_not_revoke_a_lease_and_clock_rollback_refuses() {
    let fixture = Fixture::new();
    let store = fixture.store();
    store
        .claim(&entry(), &policy(), "worker-a", 86_399, 5)
        .unwrap()
        .unwrap();
    assert!(store
        .claim(&entry(), &policy(), "worker-b", 86_400, 5)
        .unwrap()
        .is_none());
    assert!(store
        .claim(&entry(), &policy(), "worker-b", 86_404, 5)
        .unwrap()
        .is_some());
    assert!(store.claim(&entry(), &policy(), "worker-c", 0, 5).is_err());
}

#[test]
fn claim_child() {
    let Some(root) = std::env::var_os("OXA_BOARD_CLAIM_TEST_ROOT") else {
        return;
    };
    let store = Claims::open(&PathBuf::from(root)).unwrap();
    if store
        .claim(&entry(), &policy(), "worker-child", 10, 10)
        .unwrap()
        .is_some()
    {
        println!("BOARD_CLAIMED");
    }
}

#[test]
fn concurrent_processes_reserve_exactly_one_attempt() {
    let fixture = Fixture::new();
    let children: Vec<_> = (0..6)
        .map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "claim_child", "--nocapture"])
                .env_clear()
                .env("OXA_BOARD_CLAIM_TEST_ROOT", &fixture.0)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut winners = 0;
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        winners += usize::from(String::from_utf8(out.stdout).unwrap().contains("BOARD_CLAIMED"));
    }
    assert_eq!(winners, 1);
    assert_eq!(fixture.store().snapshot().unwrap()[&1].attempts, 1);
}
