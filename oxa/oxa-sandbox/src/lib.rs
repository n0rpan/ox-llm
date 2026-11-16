//! W0-FROZEN (architect-owned).
//!
//! # oxa-sandbox
//! The bwrap process sandbox behind `oxa_proto::Sandbox`.
//!
//! ## Contract (plan sec. 2)
//! `--unshare-all --die-with-parent --new-session --cap-drop ALL`; `/` read-only; the home
//! directory hidden (tmpfs) except the session cwd; a private `/tmp`; no network unless
//! `ExecSpec::net` (`--share-net`). `ExecSpec::writable == false` binds cwd read-only (plan
//! mode). No bwrap = [`Bwrap::detect`] fails with `no_sandbox` and bash refuses: there is no
//! unsandboxed fallback for tools. `!cmd` (the user's own command) uses [`run_unsandboxed`].
//! Timeout and cancel kill the whole process group; a background writer that keeps the pipe
//! open must not hang `run` (acceptance: "background writer case").

mod bwrap;
mod gitdirs;
mod layout;
mod net;
mod offline;
mod run;
mod scratch;

pub use bwrap::{account_home, bwrap_args, bwrap_args_with, without_account_home};
pub use gitdirs::{resolve as resolve_git_binds, GitBinds, MAX_ALTERNATES_DEPTH};
pub use layout::{
    expand_home, home_protected_paths, is_home_itself, is_home_or_above, is_secret_name, protected_paths,
    sensitive_paths, Layout, LayoutInputs, SETTING_ALLOW_HOME_WRITE, SETTING_READ_PATHS, SETTING_WRITE_PATHS,
};
pub use net::{sandbox_net_setting, NetSandbox, SETTING_SANDBOX_NET};
pub use offline::{no_network_argv, offline_argv};
pub use run::{
    die_with_parent, run_argv, run_argv_capture, run_argv_stdin, run_unsandboxed, run_unsandboxed_capture,
};
pub use scratch::{scratch_root, user_cache_home, Scratch};

use oxa_proto::{CancelToken, ExecResult, ExecSpec, OxaError, Sandbox};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The bwrap sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bwrap {
    /// Absolute path of the `bwrap` binary.
    pub bwrap: PathBuf,
    /// The user's home directory (hidden inside the sandbox).
    pub home: PathBuf,
    /// B2 / B3: toolchains, caches, `PATH`, the deny-list's agent directory (wave H lane
    /// `harden`; `Layout::default()` from `detect`, set by the front end).
    pub layout: Layout,
}

impl Bwrap {
    /// Find `bwrap` on `path_var` (the `PATH` value) and verify it can create a user namespace
    /// (runs `bwrap --unshare-all --ro-bind / / true`). Failure = `no_sandbox` with a hint that
    /// names the package and the sysctl to check.
    pub fn detect(path_var: &str, home: &Path) -> Result<Bwrap, OxaError> {
        bwrap::detect_bwrap(path_var, home)
    }

    /// #457: this sandbox for one session: a fresh session directory under
    /// [`Layout::scratch_root`] at `/tmp`, removed when the returned sandbox (and every clone
    /// of it) is dropped. Without a root, or when the directory cannot be created, `/tmp` is a
    /// fresh tmpfs per command and [`Sandbox::persistence_notice`] says so.
    pub fn session(&self) -> Bwrap {
        let mut b = self.clone();
        b.layout.scratch = None;
        if let Some(Ok(s)) = self.layout.scratch_root.as_deref().map(Scratch::create) {
            b.layout = b.layout.with_scratch(s);
        }
        b
    }

    /// sbx-git: bind the git data of `cwd` that lies outside it (a worktree's gitdir and common
    /// dir, the alternates of a shared clone), read from the files only. Call once, when the
    /// session's sandbox is built and [`Bwrap::layout`] is set: the binds are fixed for the
    /// session, so a command cannot change them by rewriting `.git`. Nothing is bound for a
    /// plain repository or a cwd outside a repository.
    pub fn with_git_dirs(mut self, cwd: &Path) -> Bwrap {
        self.layout.git = gitdirs::resolve(cwd, &self.home, self.layout.agent_dir.as_deref());
        let probe = ExecSpec {
            command: String::new(),
            cwd: cwd.to_path_buf(),
            writable: true,
            net: false,
            timeout_s: None,
            env: Vec::new(),
        };
        if let Some(why) = bwrap::refusal(&probe, &self.home, &self.layout) {
            self.layout.git.notes.push(why);
        }
        self
    }
}

impl Sandbox for Bwrap {
    fn home(&self) -> Option<&Path> {
        Some(&self.home)
    }
    fn persistence_notice(&self) -> Option<&str> {
        if self.layout.scratch.is_some() {
            Some(SCRATCH_NOTICE)
        } else {
            Some("Sandbox: /tmp is private scratch discarded after each command; save deliverables in the workspace. Other paths may be read-only or hidden unless explicitly bound writable.")
        }
    }
    fn for_session(&self) -> Option<Arc<dyn Sandbox>> {
        self.layout.scratch_root.as_ref()?;
        Some(Arc::new(self.session()))
    }
    fn check(&self) -> Result<(), OxaError> {
        bwrap::check_bwrap(self)
    }
    fn bwrap_args(&self, s: &ExecSpec) -> Vec<String> {
        bwrap_args_with(s, &self.home, &self.layout)
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        if let Some(why) = bwrap::refusal(s, &self.home, &self.layout) {
            return refused(why);
        }
        bwrap::create_missing_git_files(s, &self.home, &self.layout);
        self.launch(s, None, c, live, true)
    }
    fn run_streaming(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        if let Some(why) = bwrap::refusal(s, &self.home, &self.layout) {
            // The job reader sees only what went to `live`: give it the reason too.
            live(why.as_bytes());
            let mut r = refused(why);
            r.output.clear();
            return r;
        }
        bwrap::create_missing_git_files(s, &self.home, &self.layout);
        self.launch(s, None, c, live, false)
    }
    fn run_with_stdin(
        &self,
        s: &ExecSpec,
        stdin: &[u8],
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        if let Some(why) = bwrap::refusal(s, &self.home, &self.layout) {
            return refused(why);
        }
        bwrap::create_missing_git_files(s, &self.home, &self.layout);
        self.launch(s, Some(stdin), c, live, true)
    }
}

/// #457: the notice with a session directory at `/tmp`.
pub const SCRATCH_NOTICE: &str = "Sandbox: /tmp is this session's own scratch (also TMPDIR and XDG_CACHE_HOME): it persists between the commands of this session and is deleted when the session ends (/new, /resume, switching session, or quitting oxa); a resumed session starts with an empty /tmp. Save deliverables in the workspace. Other paths may be read-only or hidden unless explicitly bound writable.";

/// Environment variables that carry a secret: kept out of the bwrap command line (visible to
/// other processes) and handed over through `bwrap --args <fd>` instead.
const SECRET_ENV: &[&str] = &["GH_TOKEN"];

/// `s` without its [`SECRET_ENV`] entries, and those entries.
fn split_secrets(s: &ExecSpec) -> (ExecSpec, Vec<(String, String)>) {
    let mut public = s.clone();
    let (secret, rest): (Vec<_>, Vec<_>) = public
        .env
        .drain(..)
        .partition(|(k, _)| SECRET_ENV.contains(&k.as_str()));
    public.env = rest;
    (public, secret)
}

/// Insert `--args <fd>` before the trailing `-- bash -c <command>`: bwrap reads the NUL
/// separated `--setenv` arguments from that descriptor, after `--clearenv`, so the values
/// reach the command without being in argv.
fn insert_args_fd(argv: &mut Vec<String>, fd: i32) {
    let at = argv.len().saturating_sub(4);
    argv.splice(at..at, ["--args".to_string(), fd.to_string()]);
}

/// A pipe holding `data` (closed write end); returns the read end with `O_CLOEXEC` set, so
/// no other child spawned meanwhile inherits it. Only the bwrap child clears the flag.
fn args_pipe(data: &[u8]) -> Result<i32, String> {
    let mut fds = [-1i32; 2];
    // SAFETY: `fds` is a valid array of two ints; the fds are closed below or by the caller.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err("cannot create a pipe for the sandbox environment".to_string());
    }
    // A few hundred bytes fit in the pipe: this never blocks.
    // SAFETY: writes `data.len()` bytes from a live buffer to our own pipe.
    let n = unsafe { libc::write(fds[1], data.as_ptr().cast(), data.len()) };
    // SAFETY: the write end is ours and not used again.
    unsafe { libc::close(fds[1]) };
    if usize::try_from(n) != Ok(data.len()) {
        // SAFETY: the read end is ours.
        unsafe { libc::close(fds[0]) };
        return Err("cannot pass the sandbox environment".to_string());
    }
    Ok(fds[0])
}

impl Bwrap {
    fn launch(
        &self,
        s: &ExecSpec,
        stdin: Option<&[u8]>,
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
        capture: bool,
    ) -> ExecResult {
        let (public, secret) = split_secrets(s);
        let mut argv = vec![self.bwrap.to_string_lossy().into_owned()];
        argv.extend(bwrap_args_with(&public, &self.home, &self.layout));
        let mut args_fd = None;
        if !secret.is_empty() {
            let mut data = Vec::new();
            for (k, v) in &secret {
                for part in ["--setenv", k.as_str(), v.as_str()] {
                    data.extend_from_slice(part.as_bytes());
                    data.push(0);
                }
            }
            match args_pipe(&data) {
                Ok(fd) => {
                    insert_args_fd(&mut argv, fd);
                    args_fd = Some(fd);
                }
                Err(why) => {
                    let mut r = refused(why);
                    if !capture {
                        live(&r.output);
                        r.output.clear();
                    }
                    return r;
                }
            }
        }
        let res = run::run_argv_fd(
            &argv,
            stdin,
            args_fd,
            s.timeout_s,
            c,
            live,
            capture || stdin.is_some(),
        );
        if let Some(fd) = args_fd {
            // SAFETY: the read end is ours; the child has its own copy.
            unsafe { libc::close(fd) };
        }
        res
    }
}

#[cfg(test)]
mod secret_tests {
    use super::*;

    #[test]
    fn args_pipe_is_cloexec_and_readable() {
        let fd = args_pipe(b"abc\0").unwrap();
        // SAFETY: querying flags of a descriptor we own.
        let fl = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(fl & libc::FD_CLOEXEC != 0, "token pipe leaks across exec");
        let mut buf = [0u8; 8];
        // SAFETY: reads into a live buffer from our own fd.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), 8) };
        assert_eq!(&buf[..n as usize], b"abc\0");
        // SAFETY: our fd.
        unsafe { libc::close(fd) };
    }

    #[test]
    fn bwrap_child_still_inherits_args_fd() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("oxa-fd-inherit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("got.txt");
        let fake = dir.join("bwrap");
        // `--args N` is the 2nd-last-but-4 pair; find it and read that fd.
        std::fs::write(
            &fake,
            format!(
                "#!/bin/bash\nwhile [ \"$1\" != --args ]; do shift; done\ncat <&$2 | tr '\\0' ' ' > '{}'\n",
                out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sb = Bwrap {
            bwrap: fake,
            home: dir.clone(),
            layout: Layout::default(),
        };
        let spec = ExecSpec {
            command: "true".into(),
            cwd: dir.clone(),
            writable: true,
            net: true,
            timeout_s: Some(10),
            env: vec![("GH_TOKEN".into(), "SYNTH_FD_TOKEN".into())],
        };
        sb.run_streaming(&spec, &CancelToken::new(), &mut |_| {});
        let got = std::fs::read_to_string(&out).unwrap();
        assert!(got.contains("SYNTH_FD_TOKEN"), "{got}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A refused streaming run (a background job) reports why through `live`, so the job's
    /// output is not empty.
    #[test]
    fn refused_streaming_run_reports_the_reason_live() {
        let sb = Bwrap {
            bwrap: PathBuf::from("/nonexistent/bwrap"),
            home: std::env::temp_dir(),
            layout: Layout::default(),
        };
        let spec = ExecSpec {
            command: "true".into(),
            cwd: PathBuf::from("/nonexistent/oxa-refused-cwd"),
            writable: true,
            net: false,
            timeout_s: Some(10),
            env: Vec::new(),
        };
        let mut live = Vec::new();
        let r = sb.run_streaming(&spec, &CancelToken::new(), &mut |b| live.extend_from_slice(b));
        assert_eq!(r.exit, None);
        assert!(r.output.is_empty());
        let live = String::from_utf8_lossy(&live);
        assert!(live.contains("sandbox refused"), "{live}");
    }

    /// A fake `bwrap` that records its argv to a file, so the real launcher is exercised.
    #[test]
    fn streaming_argv_has_no_token() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("oxa-stream-argv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("argv.txt");
        let fake = dir.join("bwrap");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", out.display()),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sb = Bwrap {
            bwrap: fake,
            home: dir.clone(),
            layout: Layout::default(),
        };
        let spec = ExecSpec {
            command: "true".into(),
            cwd: dir.clone(),
            writable: true,
            net: true,
            timeout_s: Some(10),
            env: vec![("GH_TOKEN".into(), "SYNTH_STREAM_TOKEN_77".into())],
        };
        let r = sb.run_streaming(&spec, &CancelToken::new(), &mut |_| {});
        assert!(r.output.is_empty());
        let argv = std::fs::read_to_string(&out).unwrap();
        assert!(!argv.contains("SYNTH_STREAM_TOKEN_77"), "{argv}");
        assert!(argv.lines().any(|l| l == "--args"), "{argv}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn secrets_are_split_off_and_never_in_argv() {
        let spec = ExecSpec {
            command: "echo hi".into(),
            cwd: std::env::temp_dir(),
            writable: true,
            net: true,
            timeout_s: None,
            env: vec![
                ("A".into(), "1".into()),
                ("GH_TOKEN".into(), "ghp_SECRET123".into()),
            ],
        };
        let (public, secret) = split_secrets(&spec);
        assert_eq!(
            secret,
            vec![("GH_TOKEN".to_string(), "ghp_SECRET123".to_string())]
        );
        let mut argv = bwrap_args_with(&public, Path::new("/nonexistent-home"), &Layout::default());
        insert_args_fd(&mut argv, 7);
        assert!(!argv.join(" ").contains("ghp_SECRET123"), "{argv:?}");
        let n = argv.len();
        assert_eq!(&argv[n - 6..n - 4], ["--args", "7"]);
        assert_eq!(&argv[n - 4..n - 1], ["--", "bash", "-c"]);
    }
}

/// sbx-gitsym: a command the sandbox refuses to run (`bwrap::refusal`): nothing ran.
fn refused(why: String) -> ExecResult {
    ExecResult {
        output: why.into_bytes(),
        exit: None,
        timed_out: false,
    }
}

/// The sandbox used when bwrap is missing: `check` fails with `no_sandbox`, `run` returns that
/// error text as output with `exit: None`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn check(&self) -> Result<(), OxaError> {
        Err(OxaError::new(
            oxa_proto::codes::NO_SANDBOX,
            "bwrap is not available; bash is disabled",
        )
        .with_hint("install bubblewrap and make sure unprivileged user namespaces are enabled"))
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult {
            output: b"bwrap is not available; bash is disabled".to_vec(),
            exit: None,
            timed_out: false,
        }
    }
}
