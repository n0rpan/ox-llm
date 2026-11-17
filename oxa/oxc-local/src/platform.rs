//! Platform attachment policy; never a shell, wrapper process or memory-limit policy.
use crate::{lifecycle::MemorySample, memory::MemorySource};
use std::{fmt, io};

/// Attach an already owned child and validate its platform memory policy before readiness.
/// macOS supplies a separate implementation; Linux cgroup rules are not portable.
pub trait PlatformLauncher: Send + Sync {
    fn attach(&self, pid: u32) -> io::Result<MemorySample>;
}
#[derive(Debug)]
pub(crate) struct Predicate(pub &'static str);
impl fmt::Display for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Local engine check failed: {}. Use an active systemd user session with delegated cgroup v2 memory/pids controllers, or an inherited no-swap container/service; su, sudo -u, cron and logged-out sessions may have no user bus. No swap-enabled fallback is allowed.", self.0)
    }
}
impl std::error::Error for Predicate {}
pub(crate) fn refused(predicate: &'static str) -> io::Error {
    io::Error::other(Predicate(predicate))
}
pub(crate) fn no_swap(s: MemorySample) -> io::Result<MemorySample> {
    if !s.swap_limit_zero {
        return Err(refused("engine child memory.swap.max == 0"));
    }
    if s.swap_current_bytes != 0 {
        return Err(refused("engine child memory.swap.current == 0"));
    }
    Ok(s)
}

#[cfg(target_os = "linux")]
pub(crate) struct LinuxLauncher(pub MemorySource);
#[cfg(target_os = "linux")]
impl LinuxLauncher {
    fn attach_with(
        &self,
        pid: u32,
        start: impl FnOnce(u32, &str) -> io::Result<()>,
    ) -> io::Result<MemorySample> {
        if self.0.sample().is_ok_and(|s| no_swap(s).is_ok()) {
            return no_swap(self.0.sample_child(pid)?);
        }
        use std::{
            sync::atomic::{AtomicU64, Ordering},
            time::{Duration, Instant},
        };
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unit = format!(
            "oxa-local-{}-{pid}-{}.scope",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        start(pid, &unit)?;
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            if self.0.child_in_scope(pid, &unit)? {
                return no_swap(self.0.sample_child(pid)?);
            }
            if Instant::now() >= until {
                return Err(refused("engine child attached to requested no-swap scope"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
#[cfg(target_os = "linux")]
impl PlatformLauncher for LinuxLauncher {
    fn attach(&self, pid: u32) -> io::Result<MemorySample> {
        self.attach_with(pid, |pid, unit| {
            use zbus::zvariant::Value;
            let address = format!("unix:path=/run/user/{}/bus", nix::unistd::geteuid());
            let connect = || -> zbus::Result<()> {
                let bus = zbus::blocking::connection::Builder::address(address.as_str())?
                    .auth_mechanism(zbus::connection::AuthMechanism::External)
                    .method_timeout(std::time::Duration::from_secs(3))
                    .build()?;
                let properties = scope_properties(pid);
                let auxiliary: Vec<(&str, Vec<(&str, Value<'_>)>)> = Vec::new();
                bus.call_method(
                    Some("org.freedesktop.systemd1"),
                    "/org/freedesktop/systemd1",
                    Some("org.freedesktop.systemd1.Manager"),
                    "StartTransientUnit",
                    &(unit, "fail", properties, auxiliary),
                )?;
                Ok(())
            };
            connect().map_err(|_| refused("systemd user bus StartTransientUnit(MemorySwapMax=0) available"))
        })
    }
}

#[cfg(target_os = "linux")]
fn scope_properties(pid: u32) -> Vec<(&'static str, zbus::zvariant::Value<'static>)> {
    use zbus::zvariant::Value;
    vec![
        ("PIDs", Value::from(vec![pid])),
        ("MemorySwapMax", Value::from(0u64)),
        ("CollectMode", Value::from("inactive-or-failed")),
    ]
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use oxa_fake::TestEnv;
    fn fixture(inherited: bool) -> (TestEnv, LinuxLauncher) {
        let env = TestEnv::new();
        env.write("work/proc/self/cgroup", "0::/parent\n");
        env.write("work/proc/42/cgroup", "0::/child\n");
        env.write(
            "work/proc/meminfo",
            "MemAvailable: 8000000 kB\nSwapFree: 100 kB\n",
        );
        env.write("work/proc/pressure/memory", "full avg10=0\n");
        env.write("work/proc/vmstat", "oom_kill 0\n");
        for (name, zero) in [("parent", inherited), ("child", true)] {
            env.write(
                &format!("work/cgroup/{name}/memory.swap.max"),
                if zero { "0" } else { "max" },
            );
            env.write(&format!("work/cgroup/{name}/memory.swap.current"), "0");
            env.write(
                &format!("work/cgroup/{name}/memory.events"),
                "oom 1\noom_kill 2\noom_group_kill 3\n",
            );
            env.write(&format!("work/cgroup/{name}/pids.events"), "max 0\n");
        }
        let source = MemorySource::test_roots(env.work().join("proc"), env.work().join("cgroup"));
        (env, LinuxLauncher(source))
    }
    #[test]
    fn scope_request_contains_child_pid_zero_swap_and_collect_without_memorymax() {
        use zbus::zvariant::Value;
        assert_eq!(
            scope_properties(42),
            vec![
                ("PIDs", Value::from(vec![42u32])),
                ("MemorySwapMax", Value::from(0u64)),
                ("CollectMode", Value::from("inactive-or-failed"))
            ]
        );
    }
    #[test]
    fn inherited_policy_runs_direct_but_checks_child_not_parent() {
        let (env, launcher) = fixture(true);
        assert!(launcher
            .attach_with(42, |_, _| panic!("must not need user bus"))
            .is_ok());
        env.write("work/cgroup/child/memory.swap.max", "max");
        let error = launcher
            .attach_with(42, |_, _| panic!("inherited path"))
            .unwrap_err();
        assert!(error.to_string().contains("engine child memory.swap.max == 0"));
    }
    #[test]
    fn scope_attachment_uses_new_child_group_and_new_baseline() {
        let (env, launcher) = fixture(false);
        let sample = launcher
            .attach_with(42, |pid, unit| {
                assert_eq!(pid, 42);
                assert!(unit.starts_with("oxa-local-") && unit.ends_with(".scope"));
                std::fs::rename(
                    env.work().join("cgroup/child"),
                    env.work().join("cgroup").join(unit),
                )?;
                env.write("work/proc/42/cgroup", &format!("0::/{unit}\n"));
                Ok(())
            })
            .unwrap();
        assert!(sample.swap_limit_zero);
        assert_eq!(sample.oom_events, 6);
    }
    #[test]
    fn unavailable_systemd_is_explicit_and_never_falls_back() {
        let (_env, launcher) = fixture(false);
        let error = launcher
            .attach_with(42, |_, _| {
                Err(refused(
                    "systemd user bus StartTransientUnit(MemorySwapMax=0) available",
                ))
            })
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("StartTransientUnit(MemorySwapMax=0)"));
        assert!(message.contains("delegated cgroup v2"));
        assert!(message.contains("No swap-enabled fallback"));
    }
}
