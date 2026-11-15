//! Explicit private registration of an independently owned local agent.
//! This is an A2A capability, never an inference-provider key or a tool grant.
use crate::rpc_agent::{RpcAgent, RpcAgentConfig};
use oxa_a2a::{
    loopback::{Capability, Client, Limits},
    server::{Server, ServerLimits},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Foreground-owned host. Drop closes handlers and joins native task threads.
/// Use explicit shutdown to require cleanup evidence before reporting success.
/// The controller can exit without dropping this separately owned host.
pub struct AgentHost {
    server: Option<Server<RpcAgent>>,
    agent: Arc<RpcAgent>,
    directory: PathBuf,
}
impl AgentHost {
    pub fn start(config: RpcAgentConfig) -> io::Result<Self> {
        let directory = config.directory.clone();
        let agent = Arc::new(RpcAgent::new(config)?);
        let directory = directory.canonicalize()?;
        let mut random = [0u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let secret = random.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let server = Server::start_with_capability(
            agent.clone(),
            ServerLimits::default(),
            Capability::new(secret.clone())?,
        )?;
        let registration = WireRegistration {
            version: 1,
            address: server.address(),
            instance_sha256: hash(secret.as_bytes()),
            capability: secret,
        };
        crate::checkpoint::save_registration(&directory, &registration)?;
        Ok(Self {
            server: Some(server),
            agent,
            directory,
        })
    }
    /// Close handlers and join native task threads, then require explicit cleanup
    /// evidence. Drop remains best effort and does not return a cleanup proof.
    pub fn shutdown(mut self) -> io::Result<()> {
        drop(self.server.take());
        if !self.agent.cleanup_confirmed() {
            return Err(io::Error::other("agent shutdown has unconfirmed native cleanup"));
        }
        Ok(())
    }
    pub fn registration_file(&self) -> PathBuf {
        self.directory.join("registration.json")
    }
    pub fn address(&self) -> SocketAddr {
        self.server.as_ref().expect("active agent host").address()
    }
}

/// Explicitly loaded credentials, with no serialization/display implementation.
/// Debug always redacts authentication. Loading alone does not authorize a local
/// inference assertion; the operator still verifies the original agent/route.
pub struct Registration {
    address: SocketAddr,
    capability: Capability,
    instance_sha256: String,
}
impl std::fmt::Debug for Registration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Registration")
            .field("address", &self.address)
            .field("capability", &"[redacted]")
            .finish()
    }
}
impl Registration {
    pub fn load(path: &Path) -> io::Result<Self> {
        let bytes = crate::recovery::read_bounded(path, 8192)?;
        let wire: WireRegistration =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid private agent registration"))?;
        if wire.version != 1
            || !wire.address.ip().is_loopback()
            || wire.address.port() == 0
            || wire.instance_sha256 != hash(wire.capability.as_bytes())
        {
            return Err(invalid("invalid private agent registration binding"));
        }
        let capability = Capability::new(wire.capability)?;
        Ok(Self {
            address: wire.address,
            capability,
            instance_sha256: wire.instance_sha256,
        })
    }
    pub fn client(&self, limits: Limits) -> io::Result<Client> {
        Client::new(self.address, self.capability.clone(), limits)
    }
    pub fn address(&self) -> SocketAddr {
        self.address
    }
    pub fn instance_sha256(&self) -> &str {
        &self.instance_sha256
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRegistration {
    version: u32,
    address: SocketAddr,
    instance_sha256: String,
    capability: String,
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::fs;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::time::{Duration, Instant};
    fn private_env() -> TestEnv {
        let env = TestEnv::new();
        fs::set_permissions(env.root(), fs::Permissions::from_mode(0o700)).unwrap();
        env
    }
    fn wire() -> WireRegistration {
        let capability = "c".repeat(64);
        WireRegistration {
            version: 1,
            address: "127.0.0.1:12345".parse().unwrap(),
            instance_sha256: hash(capability.as_bytes()),
            capability,
        }
    }
    fn write(path: &Path, wire: &WireRegistration) {
        fs::write(path, serde_json::to_vec(wire).unwrap()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[test]
    fn explicit_registration_is_private_bounded_and_redacted() {
        let env = private_env();
        let path = env.root().join("registration.json");
        let original = wire();
        write(&path, &original);
        let registration = Registration::load(&path).unwrap();
        assert_eq!(registration.address(), original.address);
        assert_eq!(registration.instance_sha256(), original.instance_sha256);
        assert!(!format!("{registration:?}").contains(&original.capability));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Registration::load(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&path, env.root().join("symlink")).unwrap();
        assert!(Registration::load(&env.root().join("symlink")).is_err());
        fs::hard_link(&path, env.root().join("hardlink")).unwrap();
        assert!(Registration::load(&path).is_err());
        fs::remove_file(env.root().join("hardlink")).unwrap();
        let fifo = env.root().join("fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid C path inside this test's private directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert!(Registration::load(&fifo).is_err());
        assert!(started.elapsed() < Duration::from_millis(100));
        fs::write(&path, vec![b'x'; 8193]).unwrap();
        assert!(Registration::load(&path).is_err());
        write(&path, &original);
        let shared = env.root().join("shared");
        fs::DirBuilder::new().mode(0o770).create(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o770)).unwrap();
        write(&shared.join("registration.json"), &original);
        assert!(Registration::load(&shared.join("registration.json")).is_err());
    }
    #[test]
    fn malformed_registration_never_exposes_credential_or_accepts_remote_routes() {
        let env = private_env();
        let path = env.root().join("registration.json");
        let mut bad = wire();
        bad.address = "192.0.2.1:12345".parse().unwrap();
        write(&path, &bad);
        assert!(Registration::load(&path).is_err());
        bad = wire();
        bad.instance_sha256 = "0".repeat(64);
        write(&path, &bad);
        assert!(Registration::load(&path).is_err());
        let secret = wire().capability;
        fs::write(&path, serde_json::to_vec(&serde_json::json!({"version":1,"address":secret,"capability":secret,"instance_sha256":"invalid"})).unwrap()).unwrap();
        let error = Registration::load(&path).err().unwrap().to_string();
        assert!(!error.contains(&secret));
    }
    fn host_fixture() -> (TestEnv, RpcAgentConfig) {
        let (env, settings, _) = crate::supervisor::tests::fixture(&["normal"]);
        let script=fs::read_to_string(&settings.worker_binary).unwrap().replace("task=request['message']","task=request['message']\n  try: task=''.join(p.get('text','') for p in json.loads(task))\n  except (ValueError,TypeError): pass");
        fs::write(&settings.worker_binary, script).unwrap();
        let config = RpcAgentConfig {
            worker_binary: settings.worker_binary,
            bwrap_binary: settings.bwrap_binary,
            directory: env.root().join("registered-host"),
            role: "operator-fixed-role".into(),
            inference: crate::native::VerifiedLocal::register("127.0.0.1:1".parse().unwrap(), "mock".into())
                .unwrap(),
            budget: crate::rpc_agent::WorkerBudget {
                wall_time: Duration::from_secs(5),
                output_bytes: 128 * 1024,
                reported_tokens: 1000,
            },
            max_live: 2,
            max_tasks: 8,
            journal_bytes: 64 * 1024,
        };
        (env, config)
    }
    fn initial() -> oxa_a2a::wire::SendParams {
        use oxa_a2a::wire::*;
        SendParams {
            message: MessageObject::Message(Message {
                message_id: "independent-controller-message".into(),
                role: Role::User,
                parts: vec![Part::Text {
                    text: "wait-for-steer".into(),
                }],
                task_id: None,
                context_id: None,
            }),
        }
    }
    fn wait_state(client: &mut Client, id: &str, wanted: oxa_a2a::wire::TaskState) -> oxa_a2a::wire::Task {
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            let task = client.get(id).unwrap();
            if task.status.state == wanted {
                return task;
            }
            assert!(
                Instant::now() < end,
                "registered task did not reach the expected state"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn controller_exit_keeps_work_host_drop_stops_and_joins() {
        use oxa_a2a::wire::{Event, TaskState};
        use std::os::unix::fs::OpenOptionsExt;
        const CHILD: &str = "OXA_A2A_REGISTERED_SUBMITTER_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let path = std::env::var_os("OXA_A2A_REGISTRATION_FILE").unwrap();
            let registration = Registration::load(Path::new(&path)).unwrap();
            let mut client = registration.client(Limits::default()).unwrap();
            let task = client.send(initial()).unwrap();
            let reply = std::env::var_os("OXA_A2A_TASK_FILE").unwrap();
            let file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(reply)
                .unwrap();
            serde_json::to_writer(file, &task).unwrap();
            return;
        }
        let (env, config) = host_fixture();
        let directory = config.directory.clone();
        let host = AgentHost::start(config).unwrap();
        let registration = Registration::load(&host.registration_file()).unwrap();
        assert_eq!(registration.address(), host.address());
        assert_eq!(
            fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|s| s == "jsonl"))
                .count(),
            0,
            "hosting alone must not launch inference"
        );
        let reply = env.root().join("submitted-task.json");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "registration::tests::controller_exit_keeps_work_host_drop_stops_and_joins",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env(CHILD, "1")
            .env("OXA_A2A_REGISTRATION_FILE", host.registration_file())
            .env("OXA_A2A_TASK_FILE", &reply)
            .output()
            .unwrap();
        assert!(output.status.success(), "submitter output: {output:?}");
        let Event::Task(task) = serde_json::from_slice(&fs::read(reply).unwrap()).unwrap() else {
            panic!("expected registered task")
        };
        let mut client = registration.client(Limits::default()).unwrap();
        wait_state(&mut client, &task.id, TaskState::Working);
        let Event::Task(repeated) = client.send(initial()).unwrap() else {
            panic!("expected retained task")
        };
        assert_eq!(repeated.id, task.id);
        let journal = directory.join(format!("{}.jsonl", task.id));
        let before = fs::read_to_string(&journal).unwrap();
        assert_eq!(
            before
                .lines()
                .filter(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["event"] == "spawned")
                .count(),
            1
        );
        drop(host);
        let rows: Vec<serde_json::Value> = fs::read_to_string(journal)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.last().unwrap()["event"], "run_finished");
        let finished = rows.iter().find(|r| r["event"] == "finished").unwrap();
        assert_eq!(finished["details"]["cleanup"], "stopped");
        assert_eq!(finished["details"]["outcome"], "canceled");
        // Retained registration is diagnostic data, not a live-host assertion.
        assert!(Registration::load(&directory.join("registration.json")).is_ok());
        assert!(registration.client(Limits::default()).unwrap().card().is_err());
    }
    #[test]
    fn explicit_shutdown_reports_unknown_cleanup_and_an_idle_host_can_stop_cleanly() {
        use oxa_a2a::wire::{Event, TaskState};
        let (_env, config) = host_fixture();
        AgentHost::start(config).unwrap().shutdown().unwrap();
        let (_env, config) = host_fixture();
        let directory = config.directory.clone();
        let host = AgentHost::start(config).unwrap();
        let registration = Registration::load(&host.registration_file()).unwrap();
        let mut client = registration.client(Limits::default()).unwrap();
        fs::rename(&directory, directory.with_extension("retained")).unwrap();
        fs::write(&directory, "owned fixture obstruction").unwrap();
        let Event::Task(task) = client.send(initial()).unwrap() else {
            panic!("expected task")
        };
        wait_state(&mut client, &task.id, TaskState::Unknown);
        assert!(client.cancel(&task.id).is_err());
        assert!(host
            .shutdown()
            .unwrap_err()
            .to_string()
            .contains("unconfirmed native cleanup"));
    }
    #[test]
    fn active_shutdown_requires_cleanup_and_fresh_hosts_reject_old_capabilities() {
        use oxa_a2a::wire::{Event, TaskState};
        let (env, config) = host_fixture();
        let directory = config.directory.clone();
        let host = AgentHost::start(config.clone()).unwrap();
        let registration = Registration::load(&host.registration_file()).unwrap();
        let mut client = registration.client(Limits::default()).unwrap();
        let Event::Task(task) = client.send(initial()).unwrap() else {
            panic!("expected task")
        };
        wait_state(&mut client, &task.id, TaskState::Working);
        host.shutdown().unwrap();
        let journal = fs::read_to_string(directory.join(format!("{}.jsonl", task.id))).unwrap();
        assert!(journal
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .any(|r| r["event"] == "finished" && r["details"]["cleanup"] == "stopped"));
        let mut next = config;
        next.directory = env.root().join("fresh-host");
        let fresh = AgentHost::start(next).unwrap();
        let current = Registration::load(&fresh.registration_file()).unwrap();
        assert_ne!(registration.instance_sha256(), current.instance_sha256());
        let stale = Client::new(
            current.address(),
            registration.capability.clone(),
            Limits::default(),
        )
        .unwrap();
        assert!(stale.card().is_err());
        assert!(current.client(Limits::default()).unwrap().card().is_ok());
        fresh.shutdown().unwrap();
    }
}
