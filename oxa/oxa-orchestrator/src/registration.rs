//! Explicit private registration of an independently owned local agent.
//! This is an A2A capability, never an inference-provider key or a tool grant.
use crate::rpc_agent::{RpcAgent, RpcAgentConfig};
use oxa_a2a::{
    loopback::{Capability, Client, Limits},
    server::{Server, ServerLimits},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Foreground-owned host. Dropping it stops handlers and joins its native tasks.
/// The controller can exit without dropping this separately owned host.
pub struct AgentHost {
    server: Server<RpcAgent>,
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
        let server =
            Server::start_with_capability(agent, ServerLimits::default(), Capability::new(secret.clone())?)?;
        let registration = WireRegistration {
            version: 1,
            address: server.address(),
            instance_sha256: hash(secret.as_bytes()),
            capability: secret,
        };
        crate::checkpoint::save_registration(&directory, &registration)?;
        Ok(Self { server, directory })
    }
    pub fn registration_file(&self) -> PathBuf {
        self.directory.join("registration.json")
    }
    pub fn address(&self) -> SocketAddr {
        self.server.address()
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
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let _namespace = crate::namespace::protected_directory(parent)?;
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = handle.metadata()?;
        // SAFETY: geteuid only returns the effective identity.
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.len() > 8192
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(invalid(
                "registration must be an operator-owned private bounded plain single-link file",
            ));
        }
        let bytes = crate::recovery::read_handle(&handle, 8192)?;
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
        let env = TestEnv::new();
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
        let env = TestEnv::new();
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
}
