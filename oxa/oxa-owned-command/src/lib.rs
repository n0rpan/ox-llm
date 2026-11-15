//! Prepared command capability. No policy, model tools, or process supervision.
//! Credentials remain behind inherited argument descriptors and output filters.
use std::{
    fs::File,
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    path::PathBuf,
    process::Command,
    sync::Arc,
};

pub struct Prepared {
    program: PathBuf,
    args: Vec<String>,
    resources: Vec<Arc<File>>,
    stdout: Option<Redactor>,
    stderr: Option<Redactor>,
    merged: Option<Redactor>,
    eof: [bool; 2],
}
impl Prepared {
    pub fn new(program: PathBuf, args: Vec<String>) -> Self {
        Self {
            program,
            args,
            resources: vec![],
            stdout: None,
            stderr: None,
            merged: None,
            eof: [false; 2],
        }
    }
    /// Public arguments contain descriptor numbers, never their contents.
    pub fn args(&self) -> &[String] {
        &self.args
    }
    /// Add NUL-separated bubblewrap environment arguments through a private FD.
    /// CLOEXEC stays set in the parent: concurrently spawned agents cannot inherit it.
    pub fn private_env(&mut self, env: &[(String, String)], at: usize) -> io::Result<()> {
        if env.is_empty() {
            return Ok(());
        }
        if at > self.args.len() {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        for (key, value) in env {
            if key.is_empty() || key.contains(['\0', '=']) || value.contains('\0') {
                return Err(invalid());
            }
            for part in ["--setenv", key, value] {
                bytes.extend_from_slice(part.as_bytes());
                bytes.push(0);
            }
        }
        // Fits one atomic write into a fresh Linux pipe; never wait for a child
        // that has not started. Oversized operator capabilities fail closed.
        if bytes.len() > 4096 {
            return Err(invalid());
        }
        let mut fds = [-1; 2];
        // SAFETY: valid two-fd array. Both descriptors are owned below.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh descriptors uniquely transferred into File ownership.
        let reader = unsafe { File::from_raw_fd(fds[0]) };
        let mut writer = unsafe { File::from_raw_fd(fds[1]) };
        writer.write_all(&bytes)?;
        drop(writer);
        self.args
            .splice(at..at, ["--args".into(), reader.as_raw_fd().to_string()]);
        self.resources.push(Arc::new(reader));
        Ok(())
    }
    pub fn redact(&mut self, value: &str) {
        if !value.is_empty() {
            self.stdout = Some(Redactor::new(value));
            self.stderr = Some(Redactor::new(value));
            self.merged = Some(Redactor::new(value));
        }
    }
    /// The returned command owns its descriptor leases through spawn/drop.
    /// Only this child's pre-exec path clears CLOEXEC for its granted FDs.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        let fds = self.resources.clone();
        // SAFETY: pre-exec only calls async-signal-safe fcntl on our open FDs.
        unsafe {
            command.pre_exec(move || {
                for fd in &fds {
                    if libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        command
    }
    pub fn filter(&mut self, stderr: bool, bytes: &[u8], eof: bool) -> Vec<u8> {
        let filter = if stderr {
            &mut self.stderr
        } else {
            &mut self.stdout
        };
        let safe = filter
            .as_mut()
            .map_or_else(|| bytes.to_vec(), |r| r.feed(bytes, eof));
        self.eof[usize::from(stderr)] |= eof;
        // Pipe-local filters preserve matches despite interleaving. Filter the
        // retained merge too: individually incomplete pipe fragments can join
        // into a full value at EOF. Both stages precede the caller's byte cap.
        if let Some(redactor) = &mut self.merged {
            redactor.feed(&safe, self.eof == [true, true])
        } else {
            safe
        }
    }
}
fn invalid() -> io::Error {
    io::Error::other("invalid private sandbox environment")
}

// Same exact-match boundary rules as oxa-sandbox::net::StreamRedactor; owned
// state travels with the prepared capability. Each output pipe has its own state.
struct Redactor {
    value: Vec<u8>,
    pending: Vec<u8>,
}
impl Redactor {
    fn new(value: &str) -> Self {
        Self {
            value: value.as_bytes().to_vec(),
            pending: vec![],
        }
    }
    fn feed(&mut self, bytes: &[u8], eof: bool) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut at = 0;
        while at < self.pending.len() {
            let rest = &self.pending[at..];
            if rest.starts_with(&self.value) {
                out.extend_from_slice(b"[redacted]");
                at += self.value.len();
            } else if self.value.starts_with(rest) && !eof {
                break;
            } else {
                out.push(self.pending[at]);
                at += 1;
            }
        }
        self.pending.drain(..at);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redaction_survives_every_split_and_independent_pipes() {
        let value = "SYNTHETIC_BG_CAPABILITY";
        let bytes = format!("before {value} after").into_bytes();
        for split in 0..=bytes.len() {
            let mut p = Prepared::new("true".into(), vec![]);
            p.redact(value);
            let mut out = p.filter(false, &bytes[..split], false);
            assert_eq!(p.filter(true, b"other pipe", true), b"other pipe");
            out.extend(p.filter(false, &bytes[split..], true));
            assert_eq!(out, b"before [redacted] after");
        }
    }
    #[test]
    fn separate_pipe_fragments_cannot_reassemble_a_credential_in_retained_output() {
        let value = "SYNTHETIC_BG_CAPABILITY";
        for split in 1..value.len() {
            let mut p = Prepared::new("true".into(), vec![]);
            p.redact(value);
            let mut out = p.filter(false, &value.as_bytes()[..split], true);
            out.extend(p.filter(true, &value.as_bytes()[split..], true));
            assert_eq!(out, b"[redacted]");
        }
    }
    #[test]
    fn descriptor_is_private_to_the_prepared_child_and_not_argv() {
        let mut p = Prepared::new(
            "/bin/sh".into(),
            vec!["-c".into(), "cat /proc/self/fd/$2".into(), "fixture".into()],
        );
        p.private_env(&[("GH_TOKEN".into(), "SYNTHETIC_BG_CAPABILITY".into())], 3)
            .unwrap();
        assert!(!p.args().iter().any(|a| a.contains("SYNTHETIC_BG_CAPABILITY")));
        let fd = p.resources[0].as_raw_fd();
        let unrelated = Command::new("/bin/sh")
            .env_clear()
            .args(["-c", &format!("test ! -e /proc/self/fd/{fd}")])
            .status()
            .unwrap();
        assert!(
            unrelated.success(),
            "another agent must not inherit this capability"
        );
        let mut command = p.command();
        command.env_clear().env("PATH", "/usr/bin:/bin");
        drop(p); // The command keeps its descriptor lease alive through spawn.
        let output = command.output().unwrap();
        assert!(output.status.success());
        // SAFETY: the command still owns this descriptor lease in the parent.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        assert_eq!(output.stdout, b"--setenv\0GH_TOKEN\0SYNTHETIC_BG_CAPABILITY\0");
    }
    #[test]
    fn oversized_private_environment_is_refused_before_spawn() {
        let mut p = Prepared::new("true".into(), vec![]);
        assert!(p
            .private_env(&[("GH_TOKEN".into(), "x".repeat(4096))], 0)
            .is_err());
        assert!(p.args().is_empty());
    }
}
