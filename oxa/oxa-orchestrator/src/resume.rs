//! Original plan and boot-clock budget bindings. Queue replay is not implemented.
use crate::a2a_run::{A2ARunSettings, A2AWorkerSpec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::time::Duration;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RunBinding {
    pub version: u32,
    pub boot_sha256: String,
    pub started_ns: u64,
    pub deadline_ns: u64,
    pub parallelism: usize,
    pub response_reservation: usize,
    pub journal_bytes: usize,
    pub request_timeout_ns: u64,
    pub poll_interval_ns: u64,
    pub cancel_grace_ns: u64,
    pub persist_results: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerBinding {
    pub role_sha256: String,
    pub task_sha256: String,
    pub endpoint_sha256: String,
    pub wall_time_ns: u64,
    pub response_bytes: usize,
    pub reported_tokens: Option<u64>,
}
impl RunBinding {
    pub(crate) fn capture(settings: &A2ARunSettings) -> io::Result<Self> {
        let clock = Clock::read()?;
        Ok(Self {
            version: 1,
            boot_sha256: clock.boot_sha256,
            started_ns: clock.now_ns,
            deadline_ns: clock
                .now_ns
                .checked_add(nanos(settings.wall_time)?)
                .ok_or_else(|| invalid("run deadline overflow"))?,
            parallelism: settings.parallelism,
            response_reservation: settings.response_reservation,
            journal_bytes: settings.journal_bytes,
            request_timeout_ns: nanos(settings.request_timeout)?,
            poll_interval_ns: nanos(settings.poll_interval)?,
            cancel_grace_ns: nanos(settings.cancel_grace)?,
            persist_results: settings.persist_results,
        })
    }
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.version != 1
            || !valid_hash(&self.boot_sha256)
            || self.deadline_ns <= self.started_ns
            || self.deadline_ns - self.started_ns > 86_400_000_000_000
            || !(1..=32).contains(&self.parallelism)
            || self.response_reservation == 0
            || !(1024..=16 * 1024 * 1024).contains(&self.journal_bytes)
            || !(1..=1_000_000_000).contains(&self.request_timeout_ns)
            || !(1_000_000..=100_000_000).contains(&self.poll_interval_ns)
            || !(1..=2_000_000_000).contains(&self.cancel_grace_ns)
        {
            return Err(invalid("invalid recorded run budget binding"));
        }
        Ok(())
    }
    pub(crate) fn expired(&self) -> io::Result<bool> {
        let now = boot_time_ns()?;
        if now < self.started_ns {
            return Err(invalid("boot clock regressed during scheduling"));
        }
        Ok(now >= self.deadline_ns)
    }
    /// Original deadline minus downtime, including suspend; never a fresh cap.
    pub fn remaining(&self) -> io::Result<Duration> {
        self.remaining_at(Clock::read()?)
    }
    fn remaining_at(&self, clock: Clock) -> io::Result<Duration> {
        self.validate()?;
        if clock.boot_sha256 != self.boot_sha256 || clock.now_ns < self.started_ns {
            return Err(invalid("original boot clock cannot be established"));
        }
        if clock.now_ns >= self.deadline_ns {
            return Err(invalid("original run deadline has expired"));
        }
        Ok(Duration::from_nanos(self.deadline_ns - clock.now_ns))
    }
}
impl WorkerBinding {
    pub(crate) fn capture(spec: &A2AWorkerSpec) -> io::Result<Self> {
        Ok(Self {
            role_sha256: hash(spec.role.as_bytes()),
            task_sha256: hash(spec.task.as_bytes()),
            endpoint_sha256: hash(spec.agent.endpoint().as_bytes()),
            wall_time_ns: nanos(spec.wall_time)?,
            response_bytes: spec.response_bytes,
            reported_tokens: spec.reported_tokens,
        })
    }
    pub(crate) fn validate(&self) -> io::Result<()> {
        if !valid_hash(&self.role_sha256)
            || !valid_hash(&self.task_sha256)
            || !valid_hash(&self.endpoint_sha256)
            || !(1..=86_400_000_000_000).contains(&self.wall_time_ns)
            || !(1..=16 * 1024 * 1024).contains(&self.response_bytes)
            || self.reported_tokens == Some(0)
        {
            return Err(invalid("invalid recorded worker plan binding"));
        }
        Ok(())
    }
}
struct Clock {
    boot_sha256: String,
    now_ns: u64,
}
impl Clock {
    fn read() -> io::Result<Self> {
        let mut boot = String::new();
        File::open("/proc/sys/kernel/random/boot_id")?
            .take(128)
            .read_to_string(&mut boot)?;
        let boot = boot.trim();
        if boot.len() != 36
            || !boot.bytes().enumerate().all(|(i, b)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    b == b'-'
                } else {
                    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
                }
            })
        {
            return Err(invalid("invalid kernel boot identity"));
        }
        Ok(Self {
            boot_sha256: hash(boot.as_bytes()),
            now_ns: boot_time_ns()?,
        })
    }
}
pub(crate) fn boot_time_ns() -> io::Result<u64> {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: a writable timespec; CLOCK_BOOTTIME survives controller exits
    // and includes suspend, while the boot identity detects machine reboot.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, time.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful clock_gettime initialized the structure.
    let time = unsafe { time.assume_init() };
    let seconds = u64::try_from(time.tv_sec).map_err(|_| invalid("invalid boot clock"))?;
    let ns = u64::try_from(time.tv_nsec)
        .ok()
        .filter(|n| *n < 1_000_000_000)
        .ok_or_else(|| invalid("invalid boot clock"))?;
    let now_ns = seconds
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(ns))
        .ok_or_else(|| invalid("boot clock overflow"))?;
    Ok(now_ns)
}

fn nanos(duration: Duration) -> io::Result<u64> {
    u64::try_from(duration.as_nanos()).map_err(|_| invalid("duration overflow"))
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_boot_deadline_never_resets_after_downtime_or_reboot() {
        let binding = RunBinding {
            version: 1,
            boot_sha256: "a".repeat(64),
            started_ns: 100,
            deadline_ns: 200,
            parallelism: 1,
            response_reservation: 1024,
            journal_bytes: 1024,
            request_timeout_ns: 1,
            poll_interval_ns: 1_000_000,
            cancel_grace_ns: 1,
            persist_results: false,
        };
        assert_eq!(
            binding
                .remaining_at(Clock {
                    boot_sha256: "a".repeat(64),
                    now_ns: 175
                })
                .unwrap(),
            Duration::from_nanos(25)
        );
        for (boot, now_ns) in [("a", 99), ("a", 200), ("a", 201), ("b", 150)] {
            assert!(binding
                .remaining_at(Clock {
                    boot_sha256: boot.repeat(64),
                    now_ns
                })
                .is_err());
        }
    }
    #[test]
    fn every_original_worker_input_route_and_allowance_changes_the_binding() {
        use crate::a2a_run::LocalAgent;
        use oxa_a2a::loopback::{Capability, Client, Limits};
        fn spec(change: usize) -> A2AWorkerSpec {
            A2AWorkerSpec {
                id: "worker".into(),
                role: if change == 0 {
                    "changed role"
                } else {
                    "synthetic private role"
                }
                .into(),
                task: if change == 1 {
                    "changed task"
                } else {
                    "synthetic private task"
                }
                .into(),
                agent: LocalAgent::register_verified_local(
                    Client::new(
                        if change == 2 { "127.0.0.1:2" } else { "127.0.0.1:1" }
                            .parse()
                            .unwrap(),
                        Capability::generate().unwrap(),
                        Limits::default(),
                    )
                    .unwrap(),
                ),
                wall_time: Duration::from_secs(if change == 3 { 2 } else { 1 }),
                response_bytes: if change == 4 { 2048 } else { 1024 },
                reported_tokens: if change == 5 { Some(2) } else { Some(1) },
            }
        }
        let original = WorkerBinding::capture(&spec(99)).unwrap();
        original.validate().unwrap();
        for change in 0..6 {
            assert_ne!(WorkerBinding::capture(&spec(change)).unwrap(), original);
        }
        let encoded = serde_json::to_string(&original).unwrap();
        assert!(!encoded.contains("synthetic private"));
        assert!(!encoded.contains("127.0.0.1"));
        assert_eq!(serde_json::from_str::<WorkerBinding>(&encoded).unwrap(), original);
    }
}
