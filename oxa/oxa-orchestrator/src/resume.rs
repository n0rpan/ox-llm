//! Original plan and boot-clock budget bindings for explicit scheduler recovery.
use crate::a2a_run::{A2ARunSettings, A2AWorkerSpec};
use crate::{
    a2a_run::{A2ARunReport, A2AStatus, StopConfirmation},
    recovery::{ExclusiveRun, Phase, Transport},
    Control,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{atomic::AtomicBool, mpsc::Receiver, Arc};
use std::time::Duration;

/// Resume exactly the original never-started jobs under exclusive ownership.
/// Callers explicitly re-supply inputs and the ORIGINAL local agent registration.
/// A durable one-time receipt consumes the parent's queue before any child send;
/// a lost/failed child is never permission to try the parent queue again.
pub fn resume_queued(
    owner: &ExclusiveRun,
    directory: &Path,
    plan: Vec<A2AWorkerSpec>,
    controls: Receiver<Control>,
    cancel: Arc<AtomicBool>,
    observe: impl FnMut(A2AStatus),
) -> io::Result<A2ARunReport> {
    let snapshot = owner.inspected().snapshot();
    if snapshot.transport != Transport::A2a
        || snapshot.recorded_run_finished
        || snapshot.truncated_tail
        || snapshot.unlisted_workers != 0
        || plan.is_empty()
    {
        return Err(invalid("run is not eligible for queued recovery"));
    }
    let mut binding = snapshot
        .resume_binding
        .clone()
        .ok_or_else(|| invalid("original run budget binding is unavailable"))?;
    binding.remaining()?;
    let mut reports = Vec::new();
    let mut queued = std::collections::BTreeMap::new();
    for worker in &snapshot.workers {
        match worker.phase {
            Phase::Queued => {
                queued.insert(
                    worker.worker.as_str(),
                    worker
                        .resume_binding
                        .as_ref()
                        .ok_or_else(|| invalid("original worker plan binding is unavailable"))?,
                );
            }
            Phase::Finished
                if worker.recorded_stop != Some(StopConfirmation::Unconfirmed)
                    && (worker.binding.is_none()
                        || worker
                            .recorded_task_state
                            .as_ref()
                            .is_some_and(oxa_a2a::wire::TaskState::is_terminal)) =>
            {
                let path = std::path::PathBuf::from(format!("/proc/self/fd/{}", owner.directory.as_raw_fd()))
                    .join(format!("worker-{}.json", worker.worker));
                reports.push(owner.inspected().load_checkpoint(&worker.worker, &path)?);
            }
            _ => {
                return Err(invalid(
                    "started, ambiguous or stop-unconfirmed workers require reconciliation before resume",
                ))
            }
        }
    }
    if queued.len() != plan.len() {
        return Err(invalid("resume must supply exactly every never-started worker"));
    }
    let mut reserved = 0usize;
    for spec in &plan {
        let expected = queued
            .remove(spec.id.as_str())
            .ok_or_else(|| invalid("duplicate or changed queued worker identity"))?;
        if WorkerBinding::capture(spec)? != *expected {
            return Err(invalid(
                "resume changed an original worker input, route or budget",
            ));
        }
        reserved = reserved
            .checked_add(spec.response_bytes)
            .ok_or_else(|| invalid("resume reservation overflow"))?;
    }
    if reserved > binding.response_reservation {
        return Err(invalid("resume exceeds the original response reservation"));
    }
    let parent = directory
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    let _namespace = crate::namespace::protected_directory(&parent)?;
    let name = directory
        .file_name()
        .ok_or_else(|| invalid("resume needs a new destination name"))?;
    let directory = parent.join(name);
    match std::fs::symlink_metadata(&directory) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "resume destination already exists",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    binding.response_reservation = reserved;
    let child_run = format!("resume-{}", &snapshot.journal_sha256[..32]);
    crate::checkpoint::consume_resume(
        &owner.directory,
        &ResumeReceipt {
            version: 1,
            journal_sha256: snapshot.journal_sha256.clone(),
            run: snapshot.run.clone(),
            child_run: child_run.clone(),
            destination_sha256: hash(directory.as_os_str().as_bytes()),
            workers: plan.iter().map(|s| s.id.clone()).collect(),
            limits: binding.clone(),
        },
    )?;
    // Recheck after durable receipt I/O. A failure now intentionally leaves the
    // queue consumed; otherwise a storage/crash race could duplicate launches.
    let settings = A2ARunSettings {
        id: child_run,
        directory,
        persist_results: binding.persist_results,
        parallelism: binding.parallelism,
        wall_time: binding.remaining()?,
        response_reservation: reserved,
        journal_bytes: binding.journal_bytes,
        request_timeout: Duration::from_nanos(binding.request_timeout_ns),
        poll_interval: Duration::from_nanos(binding.poll_interval_ns),
        cancel_grace: Duration::from_nanos(binding.cancel_grace_ns),
    };
    let mut resumed = crate::a2a_run::run_bound(settings, plan, controls, cancel, observe, Some(binding))?;
    reports.append(&mut resumed.workers);
    reports.sort_by(|a, b| a.worker.cmp(&b.worker));
    Ok(A2ARunReport {
        run: snapshot.run.clone(),
        workers: reports,
    })
}

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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResumeReceipt {
    pub version: u32,
    pub journal_sha256: String,
    pub run: String,
    pub child_run: String,
    pub destination_sha256: String,
    pub workers: Vec<String>,
    pub limits: RunBinding,
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
            || self.journal_bytes < 1024
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
fn boot_time_ns() -> io::Result<u64> {
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
}
