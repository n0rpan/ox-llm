#[path = "support/private_dir.rs"]
mod test_dir;
use oxa_work::board::*;
use std::{collections::BTreeMap, fs, path::PathBuf, process::Command};
use test_dir::PrivateDir;

struct Fixture(PrivateDir);
impl Fixture {
    fn new() -> Self {
        Self(PrivateDir::new("board-claims"))
    }
    fn store(&self) -> Claims {
        Claims::open(self.0.path()).unwrap()
    }
    fn claim(&self, now: u64, ttl: u64) -> Option<Lease> {
        self.store()
            .claim(&entry(), &policy(), "codex:node/slot-1", now, ttl)
            .unwrap()
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
    assert_eq!(fs::read_dir(fixture.0.path()).unwrap().count(), 0);
}

#[test]
fn durable_daily_cap_and_stale_release_fencing() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let first = fixture.claim(10, 10).unwrap();
    assert!(fixture.claim(19, 10).is_none());
    assert!(store.release(1, &first).unwrap());
    let second = fixture.claim(10, 10).unwrap();
    assert!(!store.release(1, &first).unwrap());
    assert!(store.release(1, &second).unwrap());
    let third = fixture.claim(21, 10).unwrap();
    assert!(store.release(1, &third).unwrap());
    assert!(fixture.claim(22, 10).is_none());
    assert_eq!(store.snapshot().unwrap()[&1].attempts, 3);
    assert!(fixture.claim(86_400, 10).is_some());
    assert_eq!(fixture.store().snapshot().unwrap()[&1].attempts, 1);
}

#[test]
fn midnight_does_not_revoke_a_lease_and_clock_rollback_refuses() {
    let fixture = Fixture::new();
    let store = fixture.store();
    fixture.claim(86_399, 5).unwrap();
    assert!(fixture.claim(86_400, 5).is_none());
    assert!(fixture.claim(86_404, 5).is_some());
    assert!(store.claim(&entry(), &policy(), "worker-c", 0, 5).is_err());
}

#[test]
fn missing_corrupt_symlinked_or_public_state_never_resets_the_cap() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    for poison in ["missing", "corrupt", "symlink", "public"] {
        let fixture = Fixture::new();
        fixture.claim(10, 10).unwrap();
        let path = fixture.0.path().join("1.board-claim.json");
        match poison {
            "missing" => fs::remove_file(&path).unwrap(),
            "corrupt" => fs::write(&path, b"not valid state").unwrap(),
            "symlink" => {
                fs::rename(&path, fixture.0.path().join("outside")).unwrap();
                symlink(fixture.0.path().join("outside"), &path).unwrap();
            }
            _ => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
        }
        let store = fixture.store();
        assert!(store.snapshot().is_err(), "{poison}");
        assert!(
            store.claim(&entry(), &policy(), "worker", 30, 10).is_err(),
            "{poison}"
        );
    }
}

#[test]
fn lost_marker_keeps_the_cap_but_lost_state_still_fails_closed() {
    let fixture = Fixture::new();
    let store = fixture.store();
    assert!(fixture.claim(10, 10).is_some());
    // A disposable-looking marker must not decide whether the attempts on disk count.
    fs::remove_file(fixture.0.path().join("1.board-claim.lock")).unwrap();
    assert!(fixture.claim(20, 10).is_some());
    assert_eq!(store.snapshot().unwrap()[&1].attempts, 2);
    // The reverse is corruption: attempts whose state file vanished are never reset.
    let vanished = Fixture::new();
    assert!(vanished.claim(10, 10).is_some());
    fs::remove_file(vanished.0.join("1.board-claim.json")).unwrap();
    fs::remove_file(vanished.0.join("1.board-claim.lock")).unwrap();
    assert!(vanished.store().snapshot().is_err());
    assert!(vanished
        .store()
        .claim(&entry(), &policy(), "worker", 20, 10)
        .is_err());
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
fn active_and_capped_claims_repair_the_missing_creation_witness() {
    for now in [11, 21] {
        let fixture = Fixture::new();
        fixture.claim(10, 10).unwrap();
        fs::remove_file(fixture.0.path().join("1.board-claim.created")).unwrap();
        let capped = Policy {
            enabled: true,
            max_attempts_per_day: 1,
        };
        assert!(fixture
            .store()
            .claim(&entry(), &capped, "worker", now, 10)
            .unwrap()
            .is_none());
        assert!(fixture.0.path().join("1.board-claim.created").exists());
        fs::remove_file(fixture.0.path().join("1.board-claim.json")).unwrap();
        fs::remove_file(fixture.0.path().join("1.board-claim.lock")).unwrap();
        assert!(fixture.store().snapshot().is_err());
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
                .env("OXA_BOARD_CLAIM_TEST_ROOT", fixture.0.path())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut winners = 0;
    for _ in 0..256 {
        fixture.store().snapshot().unwrap();
    }
    for child in children {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        winners += usize::from(String::from_utf8(out.stdout).unwrap().contains("BOARD_CLAIMED"));
    }
    assert_eq!(winners, 1);
    assert_eq!(fixture.store().snapshot().unwrap()[&1].attempts, 1);
}
