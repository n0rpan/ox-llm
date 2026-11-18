//! #307 assignment/leases: real native CLI composition with renewable ownership.
#![cfg(all(target_os = "linux", feature = "managed-steering"))]
#[path = "common/work_repo.rs"]
mod work;

use oxa_fake::{FakeOpts, FakeServer, ScriptTurn, TestEnv};
use oxa_work::board::{assignment::*, *};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Fixture {
    env: TestEnv,
    claims: Claims,
    original: ItemLease,
    binding: NativeBinding,
    queue: PathBuf,
    config: PathBuf,
    storage: PathBuf,
    now: u64,
}

impl Fixture {
    fn new() -> Self {
        let env = TestEnv::new();
        work::setup(&env, "[]");
        let controls = env.agent_dir().join("native-lifecycle");
        let queue = controls.join("queue");
        let claims_path = controls.join("claims");
        for directory in [&controls, &queue, &claims_path] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let claims = Claims::open(&claims_path).unwrap();
        let worker: Worker = serde_json::from_value(json!({
            "id":"oxa:node/helper", "kind":"oxa", "roles":["builder"], "skills":[]
        }))
        .unwrap();
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let claimed_at = now - 120;
        claims
            .import_snapshot(
                Some(ManualAuthority::Owner),
                &BTreeMap::from([(
                    42,
                    ImportState {
                        state: AttemptState::default(),
                        excluded: false,
                    },
                )]),
                claimed_at,
            )
            .unwrap();
        let entry = Entry {
            number: 42,
            head: "synthetic-revision".into(),
            release_pinned: false,
            action: Action::BuildIssue,
            lease: None,
            attempts_today: 0,
            eligible: true,
        };
        let policy = Policy {
            enabled: true,
            ..Policy::default()
        };
        let original = claims
            .claim_item_for(&entry, &policy, &worker, "host-a", (claimed_at, 60), |_| Ok(None))
            .unwrap()
            .unwrap();
        let binding = NativeBinding::new(42, &"a".repeat(64), &original.lease, "agent/42").unwrap();
        assert!(claims.bind_native(&worker.id, &binding).unwrap());
        assert!(oxa_work::helper_queue::publish(
            &queue,
            &binding.key,
            "i42 fake agent/42 main\nSynthetic lifecycle task.\n",
            false,
            &policy,
        )
        .unwrap());
        let config = controls.join("config.json");
        fs::write(
            &config,
            json!({
                "enabled":true, "nativeExecution":true, "worker":worker, "host_id":"host-a",
                "queue":queue, "claims":claims_path, "pause":controls.join("paused"),
                "nativeSteering":{
                    "attempt":binding.native_task_id, "objective":"Complete the synthetic lifecycle task."
                }
            })
            .to_string(),
        )
        .unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
        let common = env.work().join("project/.git").canonicalize().unwrap();
        let key = format!("{:x}", Sha256::digest(common.as_os_str().as_encoded_bytes()));
        let storage = env.agent_dir().join("work").join(key);
        Self {
            env,
            claims,
            original,
            binding,
            queue,
            config,
            storage,
            now,
        }
    }

    fn run(&self, server: &FakeServer) -> (Child, Output) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_oxa"))
            .args(["work", "--native-queue", "--config"])
            .arg(&self.config)
            .current_dir(self.env.work().join("project"))
            .env_clear()
            .envs(self.env.envs(&server.url()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(90);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("native lifecycle CLI exceeded its bounded budget");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut stdout = vec![];
        let mut stderr = vec![];
        child.stdout.take().unwrap().read_to_end(&mut stdout).unwrap();
        child.stderr.take().unwrap().read_to_end(&mut stderr).unwrap();
        (
            child,
            Output {
                status,
                stdout,
                stderr,
            },
        )
    }

    fn reopened(&self) -> Claims {
        Claims::open(self.config.parent().unwrap().join("claims").as_path()).unwrap()
    }
}

fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, result);
            } else {
                result.insert(path.strip_prefix(root).unwrap().into(), fs::read(path).unwrap());
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

#[test]
fn renewed_owner_executes_and_settles_the_original_expired_receipt() {
    let fixture = Fixture::new();
    assert!(fixture.original.lease.expires_at < fixture.now);
    let renewed = fixture
        .claims
        .renew_item_for(
            42,
            &fixture.original.lease.holder,
            "host-a",
            &fixture.original,
            fixture.now - 90,
            600,
        )
        .unwrap()
        .unwrap();
    assert_eq!(renewed.lease, fixture.original.lease);
    assert!(renewed.expires_at > fixture.now);
    let reopened = fixture.reopened();
    let row = &reopened.assignments().unwrap()[&42];
    assert_eq!(row.state.lease.as_ref(), Some(&fixture.original.lease));
    assert_eq!(
        row.data.item_owner.as_ref().unwrap().expires_at,
        renewed.expires_at
    );
    assert!(row.data.native_binding.as_ref() == Some(&fixture.binding));

    let mut completion = ScriptTurn::tool(
        "done",
        "goal_done",
        r#"{"status":"complete","evidence":"Synthetic lifecycle proof."}"#,
    );
    completion.text = Some("Synthetic lifecycle complete".into());
    let server = FakeServer::start(vec![completion], FakeOpts::default()).unwrap();
    let (_, output) = fixture.run(&server);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        server
            .requests()
            .iter()
            .filter(|request| request.max_tokens > 0)
            .count(),
        1
    );
    let checkpoint: serde_json::Value = serde_json::from_slice(
        &fs::read(
            fixture
                .storage
                .join(format!("{}.json", fixture.binding.native_task_id)),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(checkpoint["record"]["id"], fixture.binding.native_task_id);
    assert_eq!(checkpoint["record"]["step"], "finished");
    let row = &fixture.reopened().assignments().unwrap()[&42];
    assert!(row.state.lease.is_none());
    assert_eq!(row.state.attempts, 1);
    assert!(row.data.native_binding.as_ref() == Some(&fixture.binding));
    assert_eq!(row.data.item_owner.as_ref().unwrap().host_id, "host-a");
    assert_eq!(
        row.data.item_owner.as_ref().unwrap().expires_at,
        renewed.expires_at
    );
    assert!(!fixture
        .queue
        .join(format!("board-{}.txt", fixture.binding.key))
        .exists());
}

#[test]
fn verified_reclaim_fences_replay_without_consuming_the_old_started_packet() {
    let fixture = Fixture::new();
    let server = FakeServer::start(vec![], FakeOpts::default()).unwrap();
    // This expired attempt launches the real child, whose managed handoff refuses
    // before a model call. The controller joins it and retains the reservation.
    let (mut child, output) = fixture.run(&server);
    assert!(!output.status.success(), "{output:?}");
    assert!(server.requests().is_empty());
    assert_eq!(
        fixture.reopened().assignments().unwrap()[&42]
            .state
            .lease
            .as_ref(),
        Some(&fixture.original.lease)
    );
    let started = fixture
        .queue
        .join("started")
        .join(format!("native-board-{}", fixture.binding.key))
        .join(format!("{}.txt", fixture.binding.native_task_id));
    assert_eq!(
        fs::read_to_string(started).unwrap(),
        "i42 fake agent/42 main\nSynthetic lifecycle task.\n"
    );
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(
            fixture
                .storage
                .join(format!("{}.lock", fixture.binding.native_task_id)),
        )
        .unwrap();
    lock.try_lock()
        .expect("the original child and inherited task lock are gone");
    assert!(fixture
        .claims
        .reclaim_item(
            42,
            Some(ManualAuthority::Lead),
            &fixture.original,
            fixture.now,
            |receipt| {
                assert_eq!(receipt, &fixture.original);
                Ok(child.try_wait().map_err(|error| error.to_string())?.is_some())
            },
        )
        .unwrap());
    drop(lock); // Replay must be fenced by Claims, not a lock held by this test.
    let queue_before = files(&fixture.queue);
    let checkpoints_before = files(&fixture.storage);
    let (_, replay) = fixture.run(&server);
    assert!(
        replay.status.success(),
        "released assignment is an idle no-op: {replay:?}"
    );
    assert!(server.requests().is_empty());
    assert_eq!(files(&fixture.queue), queue_before);
    assert_eq!(files(&fixture.storage), checkpoints_before);
    let row = &fixture.reopened().assignments().unwrap()[&42];
    assert!(row.state.lease.is_none());
    assert_eq!(row.state.attempts, 1);
    assert!(row.data.native_binding.as_ref() == Some(&fixture.binding));
}
