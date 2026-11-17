use oxa_cli::work_refill::{Consumer, Packet, Partition, Source};
use oxa_fake::TestEnv;
use oxa_work::board::{assignment::*, *};
use std::{collections::BTreeMap, fs, os::unix::fs::DirBuilderExt};

#[derive(Default)]
struct Fake {
    reads: usize,
    checks: usize,
    stale: bool,
}
impl GitHub for Fake {
    fn snapshot(&mut self) -> oxa_work::Result<Snapshot> {
        self.reads += 1;
        Ok(Snapshot {
            pull_requests: vec![PullRequest {
                number: 45,
                head: "head".into(),
                release_pinned: false,
                findings: true,
                required_ci: RequiredCi::Passed,
                mergeability: Mergeability::Mergeable,
                body: String::new(),
                removes_source: false,
                draft: false,
                reviewed: false,
            }],
            issues: [44, 46]
                .map(|number| Issue {
                    number,
                    revision: "revision".into(),
                    release_pinned: false,
                    approved: true,
                    has_open_pull_request: false,
                })
                .into(),
        })
    }
}
impl Source for Fake {
    fn revalidate(&mut self, _: &Entry) -> oxa_work::Result<bool> {
        self.checks += 1;
        Ok(!self.stale)
    }
}
const AUTHORITY: Option<ManualAuthority> = Some(ManualAuthority::Owner);
const TASK: &str = "i44 local/fake agent/issue-44 main\nSynthetic bounded existing taskfile.\n";

fn seed(claims: &Claims, excluded: bool) {
    let incoming = BTreeMap::from([(
        44,
        ImportState {
            state: AttemptState::default(),
            excluded,
        },
    )]);
    claims.import_snapshot(AUTHORITY, &incoming, 10).unwrap();
}
fn no_task(target: Consumer<'_>, source: &mut Fake, packets: &[Packet<'_>], now: u64) {
    assert!(target.once(source, packets, now, |_| Ok(())).unwrap().is_empty());
}

#[test]
fn paused_partitioned_role_checked_admission_retains_one_exact_lease() {
    let env = TestEnv::new();
    let root = env.tmp().join("claims");
    let queue = env.tmp().join("queue");
    let pause = env.tmp().join("paused");
    for path in [&root, &queue] {
        fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    }
    let claims = Claims::open(&root.canonicalize().unwrap()).unwrap();
    seed(&claims, false);
    let worker = Worker {
        id: "oxa:node/helper".into(),
        kind: WorkerKind::Oxa,
        roles: [Role::Builder].into(),
        skills: Default::default(),
    };
    let partition = Partition {
        modulus: 22,
        residues: [0].into(),
    };
    let off = Policy::default();
    let policy = Policy {
        enabled: true,
        ..Policy::default()
    };
    let packets = [Packet {
        number: 44,
        revision: "revision",
        action: Action::BuildIssue,
        task: TASK,
    }];
    let mut source = Fake::default();
    let target = |enabled| Consumer {
        repository: "example/project",
        queue: &queue,
        pause: &pause,
        claims: &claims,
        worker: &worker,
        partition: &partition,
        policy: if enabled { &policy } else { &off },
        ttl: 60,
    };
    assert!(
        target(false)
            .once(&mut source, &packets, 10, |_| panic!("off audit"))
            .unwrap()
            .is_empty()
    );
    fs::write(&pause, b"paused").unwrap();
    // Recreating the consumer simulates service startup: the persistent marker still wins.
    assert!(
        target(true)
            .once(&mut source, &packets, 10, |_| panic!("paused audit"))
            .unwrap()
            .is_empty()
    );
    assert_eq!((source.reads, source.checks), (0, 0));
    fs::remove_file(&pause).unwrap();
    assert!(
        target(true)
            .once(&mut source, &packets, 10, |_| Err("audit failed".into()))
            .is_err()
    );
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 0);
    source.stale = true;
    no_task(target(true), &mut source, &packets, 10);
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 0);
    source.stale = false;
    let receipts = target(true)
        .once(&mut source, &packets, 10, |board| {
            assert_eq!(
                board.board.entries.iter().map(|e| e.number).collect::<Vec<_>>(),
                [44]
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(receipts.len(), 1);
    let receipt = &receipts[0];
    assert_eq!(receipt.lease.holder, worker.id);
    let name = format!("board-{}.txt", receipt.key);
    assert_eq!(fs::read_to_string(queue.join(&name)).unwrap(), TASK);
    fs::rename(queue.join(&name), queue.join("started").join(name)).unwrap();
    no_task(target(true), &mut source, &packets, 80);
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 1);
    assert!(
        claims
            .release_for(44, &worker.id, &receipt.lease, 80, "verified completion")
            .unwrap()
    );
    // Retained started file dedupes before a second claim, even after fenced release.
    no_task(target(true), &mut source, &packets, 81);
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 1);
}

#[test]
fn unknown_held_owner_only_and_reviewer_work_never_publishes() {
    let env = TestEnv::new();
    let root = env.tmp().join("claims");
    let queue = env.tmp().join("queue");
    let pause = env.tmp().join("paused");
    for path in [&root, &queue] {
        fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    }
    let claims = Claims::open(&root.canonicalize().unwrap()).unwrap();
    let policy = Policy {
        enabled: true,
        ..Policy::default()
    };
    let partition = Partition {
        modulus: 22,
        residues: [0].into(),
    };
    let mut worker = Worker {
        id: "oxa:node/helper".into(),
        kind: WorkerKind::Oxa,
        roles: [Role::Builder].into(),
        skills: Default::default(),
    };
    let packets = [Packet {
        number: 44,
        revision: "revision",
        action: Action::BuildIssue,
        task: TASK,
    }];
    let mut source = Fake::default();
    for stage in 0..4 {
        if stage == 1 {
            seed(&claims, true);
        }
        if stage == 2 {
            worker.roles = [Role::Reviewer].into();
            let revision = claims.assignments().unwrap()[&44].data.revision;
            claims.clear_import_hold(44, AUTHORITY, revision).unwrap();
        }
        if stage == 3 {
            worker.roles = [Role::Builder].into();
            let revision = claims.assignments().unwrap()[&44].data.revision;
            claims
                .control(44, AUTHORITY, revision, Control::OwnerOnly(true))
                .unwrap();
        }
        let target = Consumer {
            repository: "example/project",
            queue: &queue,
            pause: &pause,
            claims: &claims,
            worker: &worker,
            partition: &partition,
            policy: &policy,
            ttl: 60,
        };
        no_task(target, &mut source, &packets, 10);
    }
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 0);
    assert!(fs::read_dir(queue).unwrap().all(|e| e.unwrap().path().is_dir()));
}
