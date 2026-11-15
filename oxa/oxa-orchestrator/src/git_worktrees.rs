//! Controller-owned Git metadata seeded solely with approved snapshot bytes.
//! No source repository/config/remote is opened; workers cannot access metadata.
use crate::workspace::{Grant, Snapshot, WorkingCopy};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::{
    fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    process::CommandExt,
};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

pub(crate) struct Pool {
    root: PathBuf,
    repositories: BTreeMap<String, Arc<Repository>>,
    attempts: usize,
}
impl Pool {
    pub(crate) fn new(root: PathBuf) -> io::Result<Self> {
        let _namespace = crate::namespace::protected_directory(
            root.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?;
        DirBuilder::new().mode(0o700).create(&root)?;
        Ok(Self {
            root,
            repositories: BTreeMap::new(),
            attempts: 0,
        })
    }
    pub(crate) fn prepare(
        &mut self,
        grant: &Arc<Grant>,
        path: &Path,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> io::Result<WorkingCopy> {
        check(deadline, cancel)?;
        if !grant.uses_git_worktrees() {
            return Err(invalid("Git preparation needs an explicit editing Git grant"));
        }
        let destination = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let path = destination.as_path();
        let parent = crate::namespace::protected_directory(
            path.parent().ok_or_else(|| invalid("missing worktree parent"))?,
        )?;
        let metadata = parent.metadata()?;
        // SAFETY: geteuid only returns the effective identity.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(invalid("worktree parent must be private and operator-owned"));
        }
        if self.attempts >= 128 {
            return Err(invalid("Git pool preparation limit exceeded"));
        }
        self.attempts += 1;
        let manifest = grant.snapshot().manifest_sha256();
        let repo = if let Some(repo) = self.repositories.get(manifest) {
            repo.clone()
        } else {
            let mut bytes = [0u8; 16];
            File::open("/dev/urandom")?.read_exact(&mut bytes)?;
            let root = self.root.join(format!(
                "snapshot-{}",
                bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ));
            let repo = Arc::new(Repository::create(root, grant.snapshot(), deadline, cancel)?);
            self.repositories.insert(manifest.into(), repo.clone());
            repo
        };
        let tree = repo.add(path, deadline, cancel)?;
        grant.snapshot().write_contents(path, cancel)?;
        Ok(WorkingCopy::from_git(path.to_path_buf(), grant.clone(), tree))
    }
}
pub(crate) fn prepare(
    pool: &Mutex<Pool>,
    grant: &Arc<Grant>,
    path: &Path,
    deadline: Instant,
    cancel: &AtomicBool,
) -> io::Result<WorkingCopy> {
    loop {
        check(deadline, cancel)?;
        match pool.try_lock() {
            Ok(mut pool) => return pool.prepare(grant, path, deadline, cancel),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(invalid("workspace controller unavailable"))
            }
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}
struct Repository {
    root: PathBuf,
    git: PathBuf,
    commit: String,
}
pub(crate) struct Worktree {
    repo: Arc<Repository>,
    pointer: Vec<u8>,
}
impl Worktree {
    pub(crate) fn mask(&self) -> PathBuf {
        self.repo.root.join("empty-metadata")
    }
    pub(crate) fn commit(&self) -> &str {
        &self.repo.commit
    }
    pub(crate) fn verify(&self, path: &Path) -> io::Result<()> {
        if metadata_pointer(path)? != self.pointer {
            return Err(invalid("worktree metadata changed"));
        }
        Ok(())
    }
}
impl Repository {
    fn create(
        root: PathBuf,
        snapshot: &Snapshot,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> io::Result<Self> {
        let git = crate::native::checked_program(Path::new("/usr/bin/git"))?;
        DirBuilder::new().mode(0o700).create(&root)?;
        DirBuilder::new().mode(0o700).create(root.join("home"))?;
        DirBuilder::new()
            .mode(0o700)
            .create(root.join("empty-template"))?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join("empty-metadata"))?;
        let mut repo = Self {
            root,
            git,
            commit: String::new(),
        };
        repo.call(
            [
                OsString::from("init"),
                "--quiet".into(),
                "--bare".into(),
                "--object-format=sha256".into(),
                "--template".into(),
                repo.root.join("empty-template").into_os_string(),
                repo.admin().into_os_string(),
            ],
            &[],
            deadline,
            cancel,
        )?;
        let mut index = Vec::new();
        for (path, text) in snapshot.entries() {
            let blob = oid(repo.call(
                [
                    "--git-dir".into(),
                    repo.admin().into_os_string(),
                    "hash-object".into(),
                    "-w".into(),
                    "--no-filters".into(),
                    "--stdin".into(),
                ],
                text.as_bytes(),
                deadline,
                cancel,
            )?)?;
            index.extend_from_slice(format!("100644 {blob}\t{}", path.as_str()).as_bytes());
            index.push(0);
        }
        repo.call(
            [
                "--git-dir".into(),
                repo.admin().into_os_string(),
                "update-index".into(),
                "-z".into(),
                "--index-info".into(),
            ],
            &index,
            deadline,
            cancel,
        )?;
        let tree = oid(repo.call(
            [
                "--git-dir".into(),
                repo.admin().into_os_string(),
                "write-tree".into(),
            ],
            &[],
            deadline,
            cancel,
        )?)?;
        repo.commit = oid(repo.call(
            [
                "--git-dir".into(),
                repo.admin().into_os_string(),
                "commit-tree".into(),
                tree.into(),
                "-m".into(),
                format!("Approved snapshot {}", snapshot.manifest_sha256()).into(),
            ],
            &[],
            deadline,
            cancel,
        )?)?;
        Ok(repo)
    }
    fn admin(&self) -> PathBuf {
        self.root.join("repository.git")
    }
    fn add(self: &Arc<Self>, path: &Path, deadline: Instant, cancel: &AtomicBool) -> io::Result<Worktree> {
        match fs::symlink_metadata(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "worktree destination already exists",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.call(
            [
                "--git-dir".into(),
                self.admin().into_os_string(),
                "worktree".into(),
                "add".into(),
                "--quiet".into(),
                "--no-checkout".into(),
                "--detach".into(),
                "--".into(),
                path.as_os_str().to_owned(),
                self.commit.clone().into(),
            ],
            &[],
            deadline,
            cancel,
        )?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        fs::set_permissions(path.join(".git"), fs::Permissions::from_mode(0o600))?;
        let pointer = metadata_pointer(path)?;
        // Populate only the index, never checkout/filter/execute source attributes.
        self.call(
            [
                "-C".into(),
                path.as_os_str().to_owned(),
                "read-tree".into(),
                self.commit.clone().into(),
            ],
            &[],
            deadline,
            cancel,
        )?;
        Ok(Worktree {
            repo: self.clone(),
            pointer,
        })
    }
    fn call(
        &self,
        args: impl IntoIterator<Item = OsString>,
        input: &[u8],
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> io::Result<Vec<u8>> {
        let mut command = Command::new(&self.git);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .env("TZ", "UTC")
            .env("HOME", self.root.join("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Orchestrator snapshot")
            .env("GIT_COMMITTER_NAME", "Orchestrator snapshot")
            .env("GIT_AUTHOR_EMAIL", "snapshot@example.invalid")
            .env("GIT_COMMITTER_EMAIL", "snapshot@example.invalid")
            .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
            .args([
                "--no-pager",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.attributesFile=/dev/null",
                "-c",
                "core.excludesFile=/dev/null",
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.fileMode=false",
                "-c",
                "protocol.allow=never",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        run(
            command,
            input,
            deadline.min(Instant::now() + Duration::from_secs(2)),
            cancel,
        )
    }
}
fn oid(output: Vec<u8>) -> io::Result<String> {
    let value = String::from_utf8(output).map_err(|_| invalid("invalid Git object identity"))?;
    let value = value.trim();
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("invalid Git object identity"));
    }
    Ok(value.into())
}
fn metadata_pointer(path: &Path) -> io::Result<Vec<u8>> {
    let bytes = crate::recovery::read_bounded(&path.join(".git"), 4096)?;
    if bytes.len() > 4096 || !bytes.starts_with(b"gitdir: ") {
        return Err(invalid("invalid worktree metadata"));
    }
    Ok(bytes)
}
struct Owned {
    child: Child,
    reaped: bool,
}
impl Owned {
    fn exited(&self) -> io::Result<bool> {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: owned unreaped Child identity and writable siginfo storage.
        // WNOWAIT observes exit without releasing PID/group identity.
        if unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful waitid initializes siginfo; zero si_pid means live.
        Ok(unsafe { info.assume_init().si_pid() } != 0)
    }
    fn finish(&mut self) -> io::Result<std::process::ExitStatus> {
        // SAFETY: the unreaped group leader reserves this process identity.
        // Fixed trusted Git operations cannot create an independently managed
        // worker. Kill any same-group pipe holder before releasing that identity.
        let result = unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        if result != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(io::Error::last_os_error());
        }
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.reaped {
            // SAFETY: unreaped process-group leader created by this runner, never a stale PID.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
fn run(mut command: Command, input: &[u8], deadline: Instant, cancel: &AtomicBool) -> io::Result<Vec<u8>> {
    check(deadline, cancel)?;
    // SAFETY: the post-fork closure calls only Linux prctl/getppid and builds
    // OS-code errors; it does not allocate, lock, or access inherited Rust state.
    let parent = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ECHILD));
            }
            Ok(())
        });
    }
    let mut owned = Owned {
        child: command.spawn()?,
        reaped: false,
    };
    let mut stdin = owned.child.stdin.take();
    let mut stdout = owned
        .child
        .stdout
        .take()
        .ok_or_else(|| invalid("missing Git output"))?;
    let mut stderr = owned
        .child
        .stderr
        .take()
        .ok_or_else(|| invalid("missing Git diagnostics"))?;
    for fd in [
        stdin.as_ref().map(AsRawFd::as_raw_fd),
        Some(stdout.as_raw_fd()),
        Some(stderr.as_raw_fd()),
    ]
    .into_iter()
    .flatten()
    {
        crate::pipes::nonblocking(fd)?;
    }
    let mut sent = 0;
    let mut output = Vec::new();
    let mut diagnostic = Vec::new();
    let mut out_eof = false;
    let mut err_eof = false;
    let mut remaining = 64 * 1024;
    loop {
        check(deadline, cancel)?;
        if sent == input.len() {
            stdin.take();
        }
        if let Some(writer) = &mut stdin {
            match writer.write(&input[sent..input.len().min(sent + 16 * 1024)]) {
                Ok(0) => return Err(invalid("Git input closed")),
                Ok(n) => sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => {
                }
                Err(e) => return Err(e),
            }
        }
        drain(&mut stdout, &mut output, &mut out_eof, &mut remaining)?;
        drain(&mut stderr, &mut diagnostic, &mut err_eof, &mut remaining)?;
        if owned.exited()? && out_eof && err_eof {
            let status = owned.finish()?;
            if !status.success() || sent != input.len() {
                return Err(invalid("isolated Git operation failed"));
            }
            return Ok(output);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn drain(
    reader: &mut impl Read,
    bytes: &mut Vec<u8>,
    eof: &mut bool,
    remaining: &mut usize,
) -> io::Result<()> {
    let mut buffer = [0; 8192];
    for _ in 0..4 {
        let allowance = buffer.len().min(remaining.saturating_add(1));
        match reader.read(&mut buffer[..allowance]) {
            Ok(0) => {
                *eof = true;
                break;
            }
            Ok(n) => {
                if n > *remaining {
                    return Err(invalid("Git output bound exceeded"));
                }
                *remaining -= n;
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
fn check(deadline: Instant, cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "workspace preparation canceled",
        ))
    } else if Instant::now() >= deadline {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "workspace preparation deadline exceeded",
        ))
    } else {
        Ok(())
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::other(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::RelativePath;
    use oxa_fake::TestEnv;
    fn private_env() -> TestEnv {
        let env = TestEnv::new();
        fs::set_permissions(env.root(), fs::Permissions::from_mode(0o700)).unwrap();
        env
    }
    fn path(name: &str) -> RelativePath {
        RelativePath::new(name.into()).unwrap()
    }
    #[test]
    fn linked_worktrees_share_only_approved_objects_and_keep_raw_bytes() {
        let env = private_env();
        fs::write(env.root().join("file space.txt"), "raw\r\nbytes\n").unwrap();
        fs::write(
            env.root().join(".gitattributes"),
            "* working-tree-encoding=UTF-16 filter=untrusted\n",
        )
        .unwrap();
        fs::write(env.root().join("not-approved.txt"), "private not approved").unwrap();
        fs::create_dir(env.root().join(".git")).unwrap();
        fs::write(env.root().join(".git/config"), "not a valid Git config\n").unwrap();
        let snapshot = Arc::new(
            Snapshot::approve(env.root(), vec![path("file space.txt"), path(".gitattributes")]).unwrap(),
        );
        let grant = Arc::new(
            Grant::editing(snapshot.clone(), vec![])
                .unwrap()
                .with_git_worktrees()
                .unwrap(),
        );
        let mut pool = Pool::new(env.root().join("controller")).unwrap();
        let cancel = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(5);
        let first = pool
            .prepare(&grant, &env.root().join("first"), deadline, &cancel)
            .unwrap();
        let second = pool
            .prepare(&grant, &env.root().join("second"), deadline, &cancel)
            .unwrap();
        assert_eq!(first.base_commit(), second.base_commit());
        assert_eq!(pool.repositories.len(), 1);
        assert_eq!(
            fs::read(first.path().join("file space.txt")).unwrap(),
            b"raw\r\nbytes\n"
        );
        assert!(!first.path().join("not-approved.txt").exists());
        let repo = pool.repositories.values().next().unwrap();
        let blob = repo
            .call(
                [
                    "--git-dir".into(),
                    repo.admin().into_os_string(),
                    "cat-file".into(),
                    "blob".into(),
                    format!("{}:file space.txt", repo.commit).into(),
                ],
                &[],
                deadline,
                &cancel,
            )
            .unwrap();
        assert_eq!(blob, b"raw\r\nbytes\n");
        let listing = repo
            .call(
                [
                    "--git-dir".into(),
                    repo.admin().into_os_string(),
                    "worktree".into(),
                    "list".into(),
                    "--porcelain".into(),
                ],
                &[],
                deadline,
                &cancel,
            )
            .unwrap();
        let listing = String::from_utf8(listing).unwrap();
        assert_eq!(listing.matches("detached").count(), 2);
        fs::write(first.path().join("file space.txt"), "proposal\n").unwrap();
        let review = first.collect("first").unwrap();
        review.verify(&snapshot, &[]).unwrap();
        assert_eq!(review.changes.len(), 1);
        assert!(second.collect("second").unwrap().changes.is_empty());
        assert_eq!(
            fs::read_to_string(env.root().join("file space.txt")).unwrap(),
            "raw\r\nbytes\n"
        );
        fs::write(first.path().join(".git"), "changed metadata").unwrap();
        assert!(first.collect("first").is_err());
    }
    #[test]
    fn cancellation_and_deadline_prevent_git_start_and_pool_wait() {
        let env = private_env();
        fs::write(env.root().join("input"), "approved").unwrap();
        let snapshot = Arc::new(Snapshot::approve(env.root(), vec![path("input")]).unwrap());
        assert!(Grant::read_only(snapshot.clone()).with_git_worktrees().is_err());
        let grant = Arc::new(
            Grant::editing(snapshot, vec![])
                .unwrap()
                .with_git_worktrees()
                .unwrap(),
        );
        let pool = Mutex::new(Pool::new(env.root().join("controller")).unwrap());
        let cancel = AtomicBool::new(true);
        assert_eq!(
            prepare(
                &pool,
                &grant,
                &env.root().join("canceled"),
                Instant::now() + Duration::from_secs(1),
                &cancel
            )
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(!env.root().join("canceled").exists());
        cancel.store(false, Ordering::Release);
        let _held = pool.lock().unwrap();
        let start = Instant::now();
        assert_eq!(
            prepare(
                &pool,
                &grant,
                &env.root().join("deadline"),
                start + Duration::from_millis(20),
                &cancel
            )
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_millis(200));
    }
    #[test]
    fn active_helper_timeout_and_cancellation_kill_and_wait() {
        let command = || {
            let mut cmd = Command::new("/usr/bin/sleep");
            cmd.arg("5")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
            cmd
        };
        let flag = AtomicBool::new(false);
        let start = Instant::now();
        assert_eq!(
            run(command(), &[], start + Duration::from_millis(20), &flag)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_millis(300));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(10));
                flag.store(true, Ordering::Release);
            });
            let start = Instant::now();
            assert_eq!(
                run(command(), &[], start + Duration::from_secs(1), &flag)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::Interrupted
            );
            assert!(start.elapsed() < Duration::from_millis(300));
        });
    }
    #[test]
    fn trusted_git_metadata_does_not_consume_the_approved_entry_budget() {
        let env = private_env();
        let mut paths = Vec::new();
        for i in 0..32 {
            let directory = format!("e{i}/p/q");
            fs::create_dir_all(env.root().join(&directory)).unwrap();
            let name = format!("{directory}/file.txt");
            fs::write(env.root().join(&name), "approved").unwrap();
            paths.push(path(&name));
        }
        let snapshot = Arc::new(Snapshot::approve(env.root(), paths).unwrap());
        let grant = Arc::new(
            Grant::editing(snapshot, vec![])
                .unwrap()
                .with_git_worktrees()
                .unwrap(),
        );
        let mut pool = Pool::new(env.root().join("controller")).unwrap();
        let flag = AtomicBool::new(false);
        let work = pool
            .prepare(
                &grant,
                &env.root().join("boundary-worktree"),
                Instant::now() + Duration::from_secs(5),
                &flag,
            )
            .unwrap();
        assert!(work.collect("boundary").unwrap().changes.is_empty());
    }
    #[test]
    fn exited_helper_with_retained_pipes_cannot_leave_a_late_writer() {
        let env = private_env();
        let marker = env.root().join("late.txt");
        let mut command = Command::new("/usr/bin/python3");
        command
            .args([
                "-I",
                "-c",
                r#"import os,sys,time
if os.fork()==0:
 time.sleep(0.45)
 open(sys.argv[1],'w').write('late')
 os._exit(0)
os._exit(0)
"#,
            ])
            .arg(&marker)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let error = run(
            command,
            &[],
            Instant::now() + Duration::from_millis(200),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !marker.exists(),
            "a retained pipe must not outlive cleanup as a writer"
        );
    }
    #[test]
    fn stdout_and_stderr_share_one_lifetime_read_allowance() {
        let mut remaining = 12;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut eof = false;
        drain(
            &mut std::io::Cursor::new(b"12345678"),
            &mut stdout,
            &mut eof,
            &mut remaining,
        )
        .unwrap();
        assert_eq!(remaining, 4);
        assert!(drain(
            &mut std::io::Cursor::new(b"12345"),
            &mut stderr,
            &mut false,
            &mut remaining
        )
        .is_err());
        assert_eq!(stdout.len() + stderr.len(), 8);
        assert_eq!(remaining, 4);
    }
    #[test]
    fn unsafe_destinations_and_exhausted_pool_attempts_never_create_another_copy() {
        let env = private_env();
        fs::write(env.root().join("input"), "approved").unwrap();
        let snapshot = Arc::new(Snapshot::approve(env.root(), vec![path("input")]).unwrap());
        let grant = Arc::new(
            Grant::editing(snapshot, vec![])
                .unwrap()
                .with_git_worktrees()
                .unwrap(),
        );
        let mut pool = Pool::new(env.root().join("controller")).unwrap();
        let flag = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(10);
        let unsafe_parent = env.root().join("unsafe");
        fs::create_dir(&unsafe_parent).unwrap();
        fs::set_permissions(&unsafe_parent, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(pool
            .prepare(&grant, &unsafe_parent.join("copy"), deadline, &flag)
            .is_err());
        assert!(pool.repositories.is_empty());
        assert_eq!(pool.attempts, 0);
        let occupied = env.root().join("occupied");
        fs::create_dir(&occupied).unwrap();
        for _ in 0..128 {
            assert!(pool.prepare(&grant, &occupied, deadline, &flag).is_err());
        }
        assert_eq!(pool.attempts, 128);
        assert!(pool
            .prepare(&grant, &env.root().join("one-more"), deadline, &flag)
            .err()
            .unwrap()
            .to_string()
            .contains("preparation limit"));
        assert!(!env.root().join("one-more").exists());
    }
}
