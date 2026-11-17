use oxa_cli::work_refill::{Consumer, Packet, Partition, Source};
use oxa_fake::TestEnv;
use oxa_work::board::{assignment::*, *};
use std::{collections::BTreeMap, fs, os::unix::fs::DirBuilderExt};
#[derive(Default)]
struct Fake<'a> {
    reads: usize,
    checks: usize,
    stale: bool,
    on_check: Option<(&'a Claims, &'a Worker, &'a Policy)>,
}
impl GitHub for Fake<'_> {
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
impl Source for Fake<'_> {
    fn revalidate(&mut self, entry: &Entry) -> oxa_work::Result<bool> {
        self.checks += 1;
        if let Some((claims, worker, policy)) = self.on_check.take() {
            let mut other = entry.clone();
            other.number = 46;
            assert!(claims.claim_for(&other, policy, worker, 10, 60)?.is_some());
        }
        Ok(!self.stale)
    }
}
const AUTHORITY: Option<ManualAuthority> = Some(ManualAuthority::Owner);
const TASK: &str = "i44 local/fake agent/issue-44 main\nSynthetic bounded existing taskfile.\n";
const PACKETS: [Packet<'static>; 1] = [Packet {
    number: 44,
    revision: "revision",
    action: Action::BuildIssue,
    task: TASK,
}];
fn seed(claims: &Claims, number: u64, excluded: bool) {
    let row = ImportState {
        state: AttemptState::default(),
        excluded,
    };
    let incoming = BTreeMap::from([(number, row)]);
    claims.import_snapshot(AUTHORITY, &incoming, 10).unwrap();
}
fn no_task(target: Consumer<'_>, source: &mut Fake, packets: &[Packet<'_>], now: u64) {
    assert!(target.once(source, packets, now, |_| Ok(())).unwrap().is_empty());
}
struct Fixture {
    _env: TestEnv,
    queue: std::path::PathBuf,
    pause: std::path::PathBuf,
    claims: Claims,
    worker: Worker,
    partition: Partition,
    policy: Policy,
}
impl Fixture {
    fn new() -> Self {
        let env = TestEnv::new();
        let root = env.tmp().join("claims");
        let queue = env.tmp().join("queue");
        let pause = env.tmp().join("paused");
        for path in [&root, &queue] {
            fs::DirBuilder::new().mode(0o700).create(path).unwrap();
        }
        Self {
            claims: Claims::open(&root.canonicalize().unwrap()).unwrap(),
            _env: env,
            queue,
            pause,
            worker: Worker {
                id: "oxa:node/helper".into(),
                kind: WorkerKind::Oxa,
                roles: [Role::Builder].into(),
                skills: Default::default(),
            },
            partition: Partition {
                modulus: 22,
                residues: [0].into(),
            },
            policy: Policy {
                enabled: true,
                ..Policy::default()
            },
        }
    }
    fn target(&self) -> Consumer<'_> {
        Consumer {
            repository: "example/project",
            queue: &self.queue,
            pause: &self.pause,
            claims: &self.claims,
            worker: &self.worker,
            partition: &self.partition,
            policy: &self.policy,
            ttl: 60,
        }
    }
}

#[test]
fn paused_partitioned_role_checked_admission_retains_one_exact_lease() {
    let fixture = Fixture::new();
    let (claims, queue, pause, worker, policy) = (
        &fixture.claims,
        &fixture.queue,
        &fixture.pause,
        &fixture.worker,
        &fixture.policy,
    );
    seed(claims, 44, false);
    let off = Policy::default();
    let mut source = Fake::default();
    let target = |enabled| Consumer {
        policy: if enabled { policy } else { &off },
        ..fixture.target()
    };
    for enabled in [false, true] {
        if enabled {
            fs::write(pause, b"paused").unwrap();
        }
        let result = target(enabled).once(&mut source, &PACKETS, 10, |_| panic!("inactive audit"));
        assert!(result.unwrap().is_empty());
    }
    assert_eq!((source.reads, source.checks), (0, 0));
    fs::remove_file(pause).unwrap();
    assert!(
        target(true)
            .once(&mut source, &PACKETS, 10, |_| Err("audit failed".into()))
            .is_err()
    );
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 0);
    source.stale = true;
    no_task(target(true), &mut source, &PACKETS, 10);
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 0);
    source.stale = false;
    seed(claims, 46, false);
    source.on_check = Some((claims, worker, policy));
    // Another authenticated reservation lands after the consumer's preliminary idle scan.
    no_task(target(true), &mut source, &PACKETS, 10);
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 0);
    let other_lease = claims.snapshot().unwrap()[&46].lease.clone().unwrap();
    assert!(
        claims
            .release_for(46, &worker.id, &other_lease, 10, "finished other ref")
            .unwrap()
    );
    let receipts = target(true)
        .once(&mut source, &PACKETS, 10, |board| {
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
    no_task(target(true), &mut source, &PACKETS, 80);
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 1);
    assert!(
        claims
            .release_for(44, &worker.id, &receipt.lease, 80, "verified completion")
            .unwrap()
    );
    // Retained started file dedupes before a second claim, even after fenced release.
    no_task(target(true), &mut source, &PACKETS, 81);
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 1);
}

#[test]
fn unknown_held_owner_only_and_reviewer_work_never_publishes() {
    let mut fixture = Fixture::new();
    let claims = &fixture.claims;
    let mut source = Fake::default();
    for stage in 0..4 {
        if stage == 1 {
            seed(claims, 44, true);
        }
        if stage == 2 {
            fixture.worker.roles = [Role::Reviewer].into();
            let revision = claims.assignments().unwrap()[&44].data.revision;
            claims.clear_import_hold(44, AUTHORITY, revision).unwrap();
        }
        if stage == 3 {
            fixture.worker.roles = [Role::Builder].into();
            let revision = claims.assignments().unwrap()[&44].data.revision;
            claims
                .control(44, AUTHORITY, revision, Control::OwnerOnly(true))
                .unwrap();
        }
        no_task(fixture.target(), &mut source, &PACKETS, 10);
    }
    assert_eq!(claims.snapshot().unwrap()[&44].attempts, 0);
    assert!(
        fs::read_dir(&fixture.queue)
            .unwrap()
            .all(|e| e.unwrap().path().is_dir())
    );
}
