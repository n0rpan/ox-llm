//! The native launcher hands its exact live assignment to the trusted worker
//! through a bounded pipe. The existing task lock remains on stdin.
use crate::{board, shadow, state, verify_task_storage, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const FD_ENV: &str = "OXA_NATIVE_STEERING_FD";
const MAX_HANDOFF: usize = 4096;
const INVALID: &str = "invalid native steering handoff";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Launch {
    claims: PathBuf,
    storage_key: String,
    binding: board::NativeBinding,
    objective: String,
}

impl Launch {
    pub(crate) fn new(
        claims: &Path,
        storage: &Path,
        binding: &board::NativeBinding,
        objective: &str,
    ) -> Result<Self> {
        binding.validate()?;
        let storage_key = storage
            .file_name()
            .and_then(|key| key.to_str())
            .filter(|key| valid_storage_key(key))
            .ok_or(INVALID)?;
        Ok(Self {
            claims: claims.into(),
            storage_key: storage_key.into(),
            binding: binding.clone(),
            objective: objective.into(),
        })
    }

    pub(crate) fn attach(&self, command: &mut Command) -> Result<()> {
        let bytes = serde_json::to_vec(self).map_err(|_| INVALID)?;
        if bytes.len() > MAX_HANDOFF {
            return Err(INVALID.into());
        }
        let (reader, mut writer) = std::io::pipe().map_err(|_| INVALID)?;
        // The handoff fits in one Linux pipe buffer. Close the writer before exec.
        writer.write_all(&bytes).map_err(|_| INVALID)?;
        drop(writer);
        command.env(FD_ENV, reader.as_raw_fd().to_string());
        // SAFETY: this closure owns the pipe reader. fcntl only changes its
        // descriptor flags in the child and is async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(reader.as_raw_fd(), libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }
}

fn valid_storage_key(key: &str) -> bool {
    key.len() == 64 && board::queue::is_object_id(key)
}

/// A live lease guard authenticated against the existing native task lock.
pub struct ManagedSteering {
    claims: board::Claims,
    binding: board::NativeBinding,
    objective: String,
}

impl ManagedSteering {
    pub fn receive(descriptor: &str, agent_dir: &Path, stdin: BorrowedFd<'_>) -> Result<Self> {
        let fd = descriptor
            .parse::<libc::c_int>()
            .ok()
            .filter(|fd| *fd > 2)
            .ok_or(INVALID)?;
        // SAFETY: fcntl accepts an unchecked integer and returns a new owned
        // descriptor on success. The original remains owned by the process.
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate < 0 {
            return Err(INVALID.into());
        }
        // SAFETY: duplicate is a new descriptor, uniquely owned here.
        let reader = unsafe { File::from_raw_fd(duplicate) };
        if !reader.metadata().map_err(|_| INVALID)?.file_type().is_fifo() {
            return Err(INVALID.into());
        }
        // SAFETY: only fcntl integer arguments. Keep the original inert across
        // later execs and refuse pipes whose writers are still open.
        if unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0
                || libc::fcntl(duplicate, libc::F_SETFL, libc::O_NONBLOCK) < 0
        } {
            return Err(INVALID.into());
        }
        let mut bytes = Vec::new();
        reader
            .take(MAX_HANDOFF as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| INVALID)?;
        if bytes.len() > MAX_HANDOFF {
            return Err(INVALID.into());
        }
        let launch: Launch = serde_json::from_slice(&bytes).map_err(|_| INVALID)?;
        Self::authenticate(launch, agent_dir, stdin)
    }

    fn authenticate(launch: Launch, agent_dir: &Path, stdin: BorrowedFd<'_>) -> Result<Self> {
        launch.binding.validate()?;
        if !valid_storage_key(&launch.storage_key) {
            return Err(INVALID.into());
        }
        let storage = agent_dir.join("work").join(&launch.storage_key);
        verify_task_storage(&storage).map_err(|_| INVALID)?;
        let expected = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(shadow::OWNER_FILE_FLAGS)
            .open(storage.join(format!("{}.lock", launch.binding.native_task_id)))
            .map_err(|_| INVALID)?;
        let inherited = File::from(stdin.try_clone_to_owned().map_err(|_| INVALID)?);
        let file = expected.metadata().map_err(|_| INVALID)?;
        let io = inherited.metadata().map_err(|_| INVALID)?;
        // SAFETY: geteuid has no preconditions.
        if !shadow::regular_file_owned_by(&file, unsafe { libc::geteuid() })
            || (file.dev(), file.ino()) != (io.dev(), io.ino())
            || !matches!(expected.try_lock(), Err(fs::TryLockError::WouldBlock))
            || inherited.try_lock().is_err()
        {
            return Err(INVALID.into());
        }
        let claims = board::Claims::open(&launch.claims)?;
        let admitted = Self {
            claims,
            binding: launch.binding,
            objective: launch.objective,
        };
        if admitted.with_live(|_| ())?.is_none() {
            return Err(INVALID.into());
        }
        Ok(admitted)
    }

    pub fn with_live<T>(&self, operation: impl FnOnce(&board::Lease) -> T) -> Result<Option<T>> {
        self.claims
            .with_live_native_lease(&self.binding, || state::now_ms() / 1000, operation)
    }

    pub fn objective(&self) -> &str {
        &self.objective
    }
}

#[cfg(test)]
#[path = "../tests/support/private_dir.rs"]
mod fixture;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::assignment::{ManualAuthority, Role, Worker, WorkerKind};
    use crate::board::{Action, AttemptState, Entry, ImportState, Policy};
    use std::collections::BTreeMap;
    use std::os::fd::AsFd;

    #[test]
    fn carrier_requires_the_original_held_lock_across_exec() {
        const ROOT: &str = "OXA_STEERING_TEST_AGENT";
        const ACCEPT: &str = "OXA_STEERING_TEST_ACCEPT";
        const OBJECTIVE: &str = "Complete synthetic assignment.";
        if let Ok(descriptor) = std::env::var(FD_ENV) {
            let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
            let admitted = ManagedSteering::receive(&descriptor, &root, std::io::stdin().as_fd());
            if std::env::var_os(ACCEPT).is_some() {
                let admitted = admitted.unwrap();
                assert_eq!(admitted.objective(), OBJECTIVE);
                assert_eq!(
                    admitted
                        .with_live(|lease| lease.holder.clone())
                        .unwrap()
                        .as_deref(),
                    Some("fixture/worker")
                );
            } else {
                assert!(admitted.is_err());
            }
            println!("verified inherited steering descriptor");
            return;
        }

        let dir = fixture::PrivateDir::new("managed-steering");
        let claims_path = dir.path().join("claims");
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&claims_path).unwrap();
        let claims = board::Claims::open(&claims_path).unwrap();
        let now = state::now_ms() / 1000;
        claims
            .import_snapshot(
                Some(ManualAuthority::Owner),
                &BTreeMap::from([(
                    1,
                    ImportState {
                        state: AttemptState::default(),
                        excluded: false,
                    },
                )]),
                now,
            )
            .unwrap();
        let worker = Worker {
            id: "fixture/worker".into(),
            kind: WorkerKind::Oxa,
            skills: Default::default(),
            roles: [Role::Builder].into(),
        };
        let entry = Entry {
            number: 1,
            head: "synthetic".into(),
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
        let lease = claims
            .claim_idle_for(&entry, &policy, &worker, now, 600)
            .unwrap()
            .unwrap();
        let key = "a".repeat(64);
        let binding = board::NativeBinding::new(1, &key, &lease, "agent/fixture").unwrap();
        assert!(claims.bind_native(&worker.id, &binding).unwrap());
        let storage = dir.path().join("work").join(&key);
        fs::create_dir_all(&storage).unwrap();
        let path = storage.join(format!("{}.lock", binding.native_task_id));
        let lock = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        lock.try_lock().unwrap();
        let launch = Launch::new(&claims_path, &storage, &binding, OBJECTIVE).unwrap();
        for (input, allowed) in [
            (lock.try_clone().unwrap(), true),
            (File::open(&path).unwrap(), false),
        ] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.env_clear().env(ROOT, dir.path()).stdin(input).args([
                "--exact",
                "managed_steering::tests::carrier_requires_the_original_held_lock_across_exec",
                "--nocapture",
            ]);
            if allowed {
                command.env(ACCEPT, "1");
            }
            launch.attach(&mut command).unwrap();
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("verified inherited steering descriptor")
            );
        }
    }
}
