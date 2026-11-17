//! Platform attachment policy; never a shell, wrapper process or memory-limit policy.
use crate::{lifecycle::MemorySample, memory::MemorySource};
use std::{fmt, io};

/// Attach an already owned child and validate its platform memory policy before readiness.
/// macOS supplies a separate implementation; Linux cgroup rules are not portable.
pub trait PlatformLauncher: Send + Sync {
    fn requires_scope(&self) -> io::Result<bool> {
        Ok(false)
    }
    fn attach(&self, pid: u32, gated: bool) -> io::Result<Attachment>;
}
#[derive(Debug)]
pub struct Attachment {
    pub sample: MemorySample,
    pub scope: Option<String>,
}
#[derive(Debug)]
pub(crate) struct Predicate(pub &'static str, &'static str);
impl fmt::Display for Predicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Local engine requires {}; {}; no swap-enabled fallback is allowed.",
            self.0, self.1
        )
    }
}
impl std::error::Error for Predicate {}
pub(crate) fn refused(predicate: &'static str) -> io::Error {
    io::Error::other(Predicate(predicate, "enable memory/pids delegation with `sudo systemctl set-property user@$(id -u).service Delegate=cpu memory pids` or use a no-swap service"))
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

pub const CHILD_GATE: &str = "--oxa-owned-local-child";
/// Internal same-PID exec gate; the parent writes permission only after child policy checks.
pub fn child_entry(args: &[std::ffi::OsString]) -> Option<io::Result<()>> {
    (args.first().is_some_and(|arg| arg == CHILD_GATE)).then(|| {
        use std::{
            io::{Read, Write},
            os::unix::process::CommandExt,
            process::{Command, Stdio},
        };
        let executable = args
            .get(1)
            .filter(|p| std::path::Path::new(p).is_absolute())
            .ok_or_else(|| refused("owned child executable is absolute"))?;
        std::io::stdout().write_all(&[0x6b])?;
        std::io::stdout().flush()?;
        let mut permission = [0];
        std::io::stdin().read_exact(&mut permission)?;
        if permission != [0xa7] {
            return Err(refused("owned child scope permission received"));
        }
        Err(Command::new(executable)
            .args(&args[2..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .exec())
    })
}

#[cfg(target_os = "linux")]
pub(crate) struct LinuxLauncher(pub MemorySource);
#[cfg(target_os = "linux")]
impl LinuxLauncher {
    fn attach_with(
        &self,
        pid: u32,
        gated: bool,
        start: impl FnOnce(u32, &str) -> io::Result<()>,
    ) -> io::Result<Attachment> {
        if !gated {
            return no_swap(self.0.sample_child(pid)?).map(|sample| Attachment { sample, scope: None });
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
                return no_swap(self.0.sample_child(pid)?).map(|sample| Attachment {
                    sample,
                    scope: Some(unit),
                });
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
    fn requires_scope(&self) -> io::Result<bool> {
        self.0
            .sample()
            .map(|s| no_swap(s).is_err())
            .map_err(|_| refused("caller cgroup v2 no-swap policy readable"))
    }
    fn attach(&self, pid: u32, gated: bool) -> io::Result<Attachment> {
        self.attach_with(pid, gated, |pid, unit| {
            use zbus::zvariant::Value;
            let address = format!("unix:path=/run/user/{}/bus", nix::unistd::geteuid());
            let bus = zbus::blocking::connection::Builder::address(address.as_str())
                .and_then(|b| {
                    b.auth_mechanism(zbus::connection::AuthMechanism::External)
                        .method_timeout(std::time::Duration::from_secs(3))
                        .build()
                })
                .map_err(|_| bus_unavailable(std::path::Path::new("/run/systemd/system").is_dir()))?;
            let properties = scope_properties(pid);
            let auxiliary: Vec<(&str, Vec<(&str, Value<'_>)>)> = Vec::new();
            bus.call_method(
                Some("org.freedesktop.systemd1"),
                "/org/freedesktop/systemd1",
                Some("org.freedesktop.systemd1.Manager"),
                "StartTransientUnit",
                &(unit, "fail", properties, auxiliary),
            )
            .map_err(|_| refused("systemd StartTransientUnit with delegated MemorySwapMax=0"))?;
            Ok(())
        })
    }
}

#[cfg(target_os = "linux")]
fn bus_unavailable(has_manager: bool) -> io::Error {
    let repair = if has_manager {
        "enable the user manager with `loginctl enable-linger $USER` and run inside its login session"
    } else {
        "use an inherited no-swap service/container, e.g. `docker run --memory=<budget> --memory-swap=<budget> <image> oxa`"
    };
    io::Error::other(Predicate(
        "systemd user bus for StartTransientUnit(MemorySwapMax=0)",
        repair,
    ))
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
        assert!(!launcher.requires_scope().unwrap());
        assert!(launcher
            .attach_with(42, false, |_, _| panic!("must not need user bus"))
            .is_ok());
        env.write("work/cgroup/child/memory.swap.max", "max");
        let error = launcher
            .attach_with(42, false, |_, _| panic!("inherited path"))
            .unwrap_err();
        assert!(error.to_string().contains("engine child memory.swap.max == 0"));
    }
    #[test]
    fn scope_attachment_uses_new_child_group_and_new_baseline() {
        let (env, launcher) = fixture(false);
        let sample = launcher
            .attach_with(42, true, |pid, unit| {
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
        assert!(sample.sample.swap_limit_zero);
        assert!(sample.scope.unwrap().ends_with(".scope"));
        assert_eq!(sample.sample.oom_events, 6);
    }
    #[test]
    fn unavailable_systemd_is_explicit_and_never_falls_back() {
        for manager in [false, true] {
            let (_env, launcher) = fixture(false);
            let error = launcher
                .attach_with(42, true, |_, _| Err(bus_unavailable(manager)))
                .unwrap_err();
            let message = error.to_string();
            assert!(message.contains("StartTransientUnit(MemorySwapMax=0)"));
            assert!(message.contains("no swap-enabled fallback"));
            assert_eq!(message.contains("loginctl enable-linger $USER"), manager);
            assert_eq!(
                message.contains("docker run --memory=<budget> --memory-swap=<budget>"),
                !manager
            );
        }
    }
}
