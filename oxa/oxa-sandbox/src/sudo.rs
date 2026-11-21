//! Bind the password broker to the child actually spawned by the host runner.
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// The process that connected, rather than whoever happens to have its numeric PID now.
#[cfg(target_os = "linux")]
pub struct PeerLifetime(std::os::fd::OwnedFd);

#[cfg(target_os = "linux")]
impl PeerLifetime {
    pub fn from_socket(stream: &std::os::unix::net::UnixStream) -> Option<Self> {
        use std::os::fd::FromRawFd;
        let mut fd: libc::c_int = -1;
        let mut len = std::mem::size_of_val(&fd) as libc::socklen_t;
        // SAFETY: the stream is live and the output is a descriptor-sized buffer.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&mut fd as *mut libc::c_int).cast(),
                &mut len,
            )
        };
        if result != 0 || fd < 0 {
            return None;
        }
        // SAFETY: SO_PEERPIDFD returned a new descriptor owned by the caller.
        let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        (len as usize == std::mem::size_of::<libc::c_int>()).then_some(Self(fd))
    }

    pub fn is_live(&self) -> bool {
        let mut poll = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: the descriptor is live and poll gets one writable entry, without waiting.
        (unsafe { libc::poll(&mut poll, 1, 0) }) == 0
    }
}

/// A private launch selector carried in ExecSpec, not a credential or a child env var.
pub const SUDO_LAUNCH_ENV: &str = "OXA_SUDO_LAUNCH";
pub(crate) const SYSTEM_SUDO: &str = "/usr/bin/sudo";

static NEXT: AtomicU64 = AtomicU64::new(0);
static LAUNCHES: std::sync::LazyLock<Mutex<HashMap<String, Weak<SudoLaunch>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

pub struct SudoLaunch {
    selector: String,
    helper: PathBuf,
    command: String,
    argv: Vec<String>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    closed: bool,
    child: Option<(u32, Option<File>)>,
    claimed: bool,
}

impl SudoLaunch {
    pub fn register(helper: PathBuf, command: String, argv: Vec<String>) -> Option<Arc<Self>> {
        let selector = NEXT.fetch_add(1, Ordering::Relaxed).to_string();
        let launch = Arc::new(Self {
            selector,
            helper,
            command,
            argv,
            state: Mutex::default(),
        });
        LAUNCHES
            .lock()
            .ok()?
            .insert(launch.selector.clone(), Arc::downgrade(&launch));
        Some(launch)
    }

    pub fn selector(&self) -> &str {
        &self.selector
    }

    pub fn revoke(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
        }
        if let Ok(mut launches) = LAUNCHES.lock() {
            launches.remove(&self.selector);
        }
    }

    pub(crate) fn resolve(selector: &str, command: &str) -> Option<Arc<Self>> {
        let launch = LAUNCHES.lock().ok()?.get(selector)?.upgrade()?;
        (launch.command == command).then_some(launch)
    }

    pub(crate) fn helper(&self) -> &Path {
        &self.helper
    }

    pub(crate) fn command(&self) -> Command {
        // Do not go through shell startup, a PATH shim or nested shell commands.
        let mut cmd = Command::new(SYSTEM_SUDO);
        cmd.args(&self.argv);
        cmd
    }

    pub(crate) fn spawn(&self, cmd: &mut Command) -> std::io::Result<Child> {
        // Hold the lock across spawn and registration so an immediate askpass connection
        // cannot race the runner. spawn returns at exec, not when sudo exits.
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("sudo launch lock"))?;
        if state.closed || state.child.is_some() {
            return Err(std::io::Error::other("sudo launch is no longer available"));
        }
        // SAFETY: setsid is async-signal-safe. The session leader is also the process
        // group leader, and sudo cannot open the TUI's controlling terminal.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        // The child has not been reaped. Hold its proc directory, not just the numeric
        // PID: a later process reusing the number cannot resolve stat through this fd.
        state.child = Some((child.id(), File::open(format!("/proc/{}", child.id())).ok()));
        Ok(child)
    }

    pub fn claim(&self, parent: u32) -> bool {
        self.state.lock().is_ok_and(|mut state| {
            if state.claimed || !state.matches(parent) {
                return false;
            }
            state.claimed = true;
            true
        })
    }

    /// Run a bounded delivery without allowing the foreground scope to close in between.
    pub fn deliver(&self, parent: u32, send: impl FnOnce()) -> bool {
        self.state.lock().is_ok_and(|state| {
            if !state.claimed || !state.matches(parent) {
                return false;
            }
            send();
            true
        })
    }
}

impl State {
    fn matches(&self, parent: u32) -> bool {
        let Some((pid, Some(dir))) = &self.child else {
            return false;
        };
        !self.closed
            && *pid == parent
            && std::fs::read_to_string(format!("/proc/self/fd/{}/stat", dir.as_raw_fd())).is_ok()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn only_the_selected_live_child_has_one_claim() {
        let register = || {
            SudoLaunch::register(
                "/unused/oxa-askpass".into(),
                "sudo true".into(),
                vec!["true".into()],
            )
            .unwrap()
        };
        let open = register();
        let other = register();
        assert!(SudoLaunch::resolve(open.selector(), "sudo false").is_none());
        assert!(
            !open.claim(std::process::id()),
            "registration alone admits nobody"
        );
        let launch = SudoLaunch::resolve(open.selector(), "sudo true").unwrap();
        // Stand in for the launched child without invoking sudo or reading a password.
        let mut child = launch.spawn(Command::new("cat").stdin(Stdio::piped())).unwrap();
        let mut second = other.spawn(Command::new("cat").stdin(Stdio::piped())).unwrap();
        let pid = child.id();
        let foreign = open.claim(second.id()) || other.claim(pid);
        let first = open.claim(pid);
        let duplicate = open.claim(pid);
        let mut delivered = false;
        let active = open.deliver(pid, || delivered = true);
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        assert!(!open.deliver(pid, || panic!("delivery to a reaped child")));
        let second_claim = other.claim(second.id());
        other.revoke();
        let revoked = !other.deliver(second.id(), || {});
        drop(second.stdin.take());
        assert!(second.wait().unwrap().success());
        open.revoke();
        assert!(SudoLaunch::resolve(open.selector(), "sudo true").is_none());
        assert!(
            second_claim && revoked,
            "closing the foreground scope revokes delivery"
        );
        assert!(!foreign, "a process cannot use another call's grant");
        assert!(
            first && active && delivered && !duplicate,
            "the live child gets one claim"
        );
    }
}
