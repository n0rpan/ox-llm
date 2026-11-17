//! Event-driven child waiting with one wall deadline and two escalation stages.
use crate::Result;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

pub(crate) struct Waited {
    pub status: ExitStatus,
    pub deadline: bool,
    pub failure: Option<String>,
}

struct Sender(Option<Child>);
impl Drop for Sender {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

pub(crate) fn wait(
    child: &mut Child,
    notice: &Receiver<Option<String>>,
    max_time: Duration,
    grace: Duration,
    kill_grace: Duration,
    terminate: &Path,
) -> Result<Waited> {
    let term_at = Instant::now()
        .checked_add(max_time)
        .and_then(|at| at.checked_add(grace))
        .ok_or("worker deadline is too large")?;
    let kill_at = term_at
        .checked_add(kill_grace)
        .ok_or("worker deadline is too large")?;
    let mut deadline = false;
    let mut streams_ended = false;
    let mut sender = Sender(None);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok(Waited {
                status,
                deadline,
                failure: None,
            });
        }
        let now = Instant::now();
        if !deadline && now >= term_at {
            // The child remains unreaped: its positive PID cannot be reused.
            // This absolute executable was validated before worker admission.
            sender.0 = Command::new(terminate)
                .args(["-TERM", "--", &child.id().to_string()])
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
            deadline = true;
        }
        if now >= kill_at {
            let _ = child.kill();
            let status = child.wait().map_err(|e| e.to_string())?;
            return Ok(Waited {
                status,
                deadline: true,
                failure: None,
            });
        }
        let pause = (if deadline { kill_at } else { term_at }).saturating_duration_since(Instant::now());
        let pause = if streams_ended {
            pause.min(Duration::from_millis(20))
        } else {
            pause
        };
        match notice.recv_timeout(pause) {
            Ok(Some(error)) => {
                let _ = child.kill();
                let status = child.wait().map_err(|e| e.to_string())?;
                return Ok(Waited {
                    status,
                    deadline,
                    failure: Some(error),
                });
            }
            Ok(None) => streams_ended = true,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                streams_ended = true;
                std::thread::sleep(pause.min(Duration::from_millis(20)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::{
        fs::{DirBuilderExt, PermissionsExt},
        process::ExitStatusExt,
    };
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "oxa-worker-deadline-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
            ));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            Self(root)
        }
        fn child(&self, script: &str) -> Owned {
            let child = Command::new("/bin/sh")
                .args(["-c", script, "deadline-fixture"])
                .arg(self.0.join("ready"))
                .arg(self.0.join("term"))
                .env_clear()
                .env("HOME", &self.0)
                .env("PATH", "/usr/bin:/bin")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let child = Owned(child);
            let end = Instant::now() + Duration::from_secs(5);
            while !self.0.join("ready").exists() {
                assert!(Instant::now() < end, "child installed its signal handler");
                std::thread::sleep(Duration::from_millis(5));
            }
            child
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    struct Owned(Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    fn notices(child: &mut Owned) -> (mpsc::Sender<Option<String>>, Receiver<Option<String>>) {
        let (sender, receiver) = mpsc::channel();
        let done = sender.clone();
        let mut pipe = child.0.stderr.take().unwrap();
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut pipe, &mut std::io::sink());
            let _ = done.send(None);
        });
        (sender, receiver)
    }

    #[test]
    fn wall_deadline_sends_sigterm_after_the_wrap_up_grace() {
        let fixture = Fixture::new();
        let mut child = fixture.child("trap 'exit 42' TERM\nprintf ready > \"$1\"\nwhile :; do :; done\n");
        let (_sender, receiver) = notices(&mut child);
        let started = Instant::now();
        let result = wait(
            &mut child.0,
            &receiver,
            Duration::from_millis(30),
            Duration::from_millis(30),
            Duration::from_secs(2),
            Path::new("/usr/bin/kill"),
        )
        .unwrap();
        assert!(result.deadline);
        assert_eq!(result.status.code(), Some(42));
        assert!(result.failure.is_none());
        assert!(started.elapsed() >= Duration::from_millis(60));
    }

    #[test]
    fn a_child_that_keeps_running_after_sigterm_is_killed_even_after_stream_eof() {
        let fixture = Fixture::new();
        let mut child =
            fixture.child("exec 2>/dev/null\ntrap 'printf term > \"$2\"' TERM\nprintf ready > \"$1\"\nwhile :; do :; done\n");
        let (_sender, receiver) = notices(&mut child);
        let started = Instant::now();
        let result = wait(
            &mut child.0,
            &receiver,
            Duration::from_millis(30),
            Duration::from_millis(30),
            Duration::from_millis(200),
            Path::new("/usr/bin/kill"),
        )
        .unwrap();
        assert!(result.deadline);
        assert_eq!(result.status.signal(), Some(9));
        assert_eq!(fs::read_to_string(fixture.0.join("term")).unwrap(), "term");
        assert!(started.elapsed() >= Duration::from_millis(260));
    }

    #[test]
    fn a_checkpoint_failure_terminates_the_current_child_without_waiting_for_the_deadline() {
        let fixture = Fixture::new();
        let mut child = fixture.child("printf ready > \"$1\"\nwhile :; do :; done\n");
        let (sender, receiver) = notices(&mut child);
        sender.send(Some("synthetic checkpoint failure".into())).unwrap();
        let started = Instant::now();
        let result = wait(
            &mut child.0,
            &receiver,
            Duration::from_secs(30),
            Duration::from_secs(60),
            Duration::from_secs(10),
            Path::new("/usr/bin/kill"),
        )
        .unwrap();
        assert!(!result.deadline);
        assert_eq!(result.status.signal(), Some(9));
        assert_eq!(result.failure.as_deref(), Some("synthetic checkpoint failure"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_stuck_term_sender_cannot_delay_the_final_kill() {
        let fixture = Fixture::new();
        let sender = fixture.0.join("slow-terminate");
        fs::write(&sender, "#!/bin/sh\n/usr/bin/sleep 2\n").unwrap();
        fs::set_permissions(&sender, fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = fixture.child("printf ready > \"$1\"\nwhile :; do :; done\n");
        let (_sender, receiver) = notices(&mut child);
        let started = Instant::now();
        let result = wait(
            &mut child.0,
            &receiver,
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_millis(100),
            &sender,
        )
        .unwrap();
        assert_eq!(result.status.signal(), Some(9));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "TERM sender blocked SIGKILL"
        );
    }
}
