//! `done` setting (issue #104): shell commands oxa runs itself before it reports completion.
//! A run that edited files (`edit` / `write` succeeded, or a `bash`, sub-agent, MCP or extension
//! tool call changed the work folder) and is about to stop runs them in the session sandbox; a failure goes back to the model as a user
//! message, at most [`MAX_ROUNDS`] times. An entry `in <dir>: <command>` runs in `<dir>`, a folder
//! of the session folder, instead of in the session folder itself.
use crate::exec::CallResult;
use crate::SessionState;
use oxa_proto::{
    shown_why, AgentEvent, EntryBody, EventMessage, EventSink, ExecSpec, Level, OxaError, ToolCall, Ui,
};
use oxa_tools::strip_ansi;
use std::path::{Component, Path, PathBuf};

/// Automatic fix rounds after a failed check; the next failure ends the run.
const MAX_ROUNDS: u32 = 2;
/// Seconds one command may run.
const TIMEOUT_S: u32 = 600;
/// Bytes of output kept per command (the tail).
const OUT_CAP: usize = 4096;

pub(crate) enum CheckObservation {
    Passed,
    Failed {
        identity: String,
        rounds: u32,
        evidence: Option<String>,
    },
}

#[derive(Default)]
pub(crate) struct DoneState {
    /// A file tool succeeded since the last check.
    dirty: bool,
    rounds: u32,
    pub(crate) observation: Option<CheckObservation>,
    /// Fingerprint of the work folder at the last look (`None`: too big to look at).
    tree: Option<u64>,
    /// The "prove it" state of the run (B4a).
    pub(crate) proof: Proof,
}

/// Whether the run changed files and no check has passed since the last change (B4a). Only
/// tracks the state: nothing is asked from here.
#[derive(Default)]
pub(crate) struct Proof {
    /// A file changed in this run: a successful edit or write, or a bash-like call that changed
    /// the work folder.
    edited: bool,
    /// A passing check ran after the last change.
    checked: bool,
}

impl Proof {
    fn changed(&mut self) {
        self.edited = true;
        self.checked = false;
    }

    fn passed(&mut self) {
        self.checked = true;
    }

    /// The run changed files and no check has passed since the last change. Read by the
    /// "prove it" turn (B4b); until then only the tests read it.
    #[allow(dead_code)]
    pub(crate) fn unproven(&self) -> bool {
        self.edited && !self.checked
    }
}

/// Seconds a git probe may run before the fingerprint gives up (`None`: checks stay as they were).
const PROBE_TIMEOUT_S: u64 = 20;

/// Hash of the work folder's git file list (index entries and untracked files) with each file's
/// size, times, inode and mode. Outside a git repo, without git, or on a timeout it is a bounded
/// walk of the folder ([`walk_fingerprint`]).
fn tree_fingerprint(cwd: &Path) -> Option<u64> {
    git_fingerprint(cwd).or_else(|| walk_fingerprint(cwd))
}

/// Entries the non-git walk looks at before it gives up (`None`: no checks from tree changes).
const WALK_CAP: usize = 20_000;

/// Hash of every readable file's relative path, size and modification time under `cwd`, plus
/// contents for regular files up to 1 MiB. Does not follow symlinks or descend
/// into `.git`, `node_modules`, `target` or unreadable subdirectories. `None` above [`WALK_CAP`]
/// entries or when `cwd` cannot be read.
fn walk_fingerprint(cwd: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut seen = 0usize;
    let mut stack = vec![cwd.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) if dir == cwd => return None,
            Err(_) => continue,
        };
        let mut entries = walk_entries(entries, &mut seen)?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            let Ok(m) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            name.hash(&mut h);
            dir.strip_prefix(cwd).ok().hash(&mut h);
            m.len().hash(&mut h);
            m.modified().ok().hash(&mut h);
            if m.is_file() && m.len() <= (1 << 20) {
                std::fs::read(e.path()).ok().hash(&mut h);
            }
            if m.is_dir() && !matches!(name.to_str(), Some(".git" | "node_modules" | "target")) {
                stack.push(e.path());
            }
        }
    }
    Some(h.finish())
}

fn walk_entries<T>(entries: impl Iterator<Item = std::io::Result<T>>, seen: &mut usize) -> Option<Vec<T>> {
    let mut collected = Vec::new();
    for entry in entries {
        *seen += 1;
        if *seen > WALK_CAP {
            return None;
        }
        if let Ok(entry) = entry {
            collected.push(entry);
        }
    }
    Some(collected)
}

/// The git part of [`tree_fingerprint`]; `None` outside a git repo, without git, or on a timeout.
/// The probes run on the host, so they only list files (`ls-files`): nothing that can start a
/// repository-configured program (`status` and `diff` run clean filters, textconv, external
/// diff). Files are stat-ed here instead of diffed by git. A same-size in-place rewrite that
/// restores mtime within one ctime tick can be missed on coarse-timestamp filesystems; file
/// tools mark edits directly, independently of this best-effort bash probe.
fn git_fingerprint(cwd: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let git = |args: &[&str]| -> Option<Vec<u8>> {
        let mut child = std::process::Command::new("git")
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(cwd)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        let mut out = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = std::io::Read::read_to_end(&mut out, &mut v);
            v
        });
        let t0 = std::time::Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait().ok()? {
                break s;
            }
            if t0.elapsed().as_secs() >= PROBE_TIMEOUT_S {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let v = reader.join().ok()?;
        status.success().then_some(v)
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // `-s`: mode, object id and stage of every index entry (staging changes the hash).
    git(&["ls-files", "-z", "-s"])?.hash(&mut h);
    let files = git(&["ls-files", "-z", "--cached", "--others", "--exclude-standard"])?;
    for name in files.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let path = cwd.join(std::ffi::OsStr::from_bytes(name));
        name.hash(&mut h);
        match std::fs::symlink_metadata(&path) {
            Ok(m) => {
                m.len().hash(&mut h);
                m.modified().ok().hash(&mut h);
                // Inode, mode and change time catch a rewrite that keeps size and mtime.
                m.ino().hash(&mut h);
                m.mode().hash(&mut h);
                (m.ctime(), m.ctime_nsec()).hash(&mut h);
                // Do not read file contents: the metadata avoids repository-wide I/O and
                // stays stable as unchanged files age, unlike a recency-gated content hash.
            }
            Err(_) => 0u8.hash(&mut h),
        }
    }
    Some(h.finish())
}

impl DoneState {
    fn observe_tree(&mut self, now: Option<u64>, file_edit: bool) {
        // In a mixed batch, note_proof has already used call order to decide whether a check
        // followed the edit. Keep that decision and only synchronize the observed tree here.
        if file_edit {
            self.tree = now;
        } else if now != self.tree {
            self.dirty = true;
            self.proof.changed();
            self.tree = now;
        }
    }

    pub(crate) fn note_edits(&mut self, results: &[CallResult]) {
        if results
            .iter()
            .any(|r| !r.is_error && (r.tool_name == "edit" || r.tool_name == "write"))
        {
            self.dirty = true;
        }
    }

    /// One batch, in call order (`calls` and `results` line up): a successful edit or write is a
    /// change; a successful bash call that runs a test or check after the last change proves it.
    pub(crate) fn note_proof(&mut self, calls: &[ToolCall], results: &[CallResult]) {
        for (call, res) in calls.iter().zip(results) {
            if res.is_error {
                continue;
            }
            match res.tool_name.as_str() {
                "edit" | "write" => self.proof.changed(),
                "bash" => {
                    let args = serde_json::from_str::<serde_json::Value>(&call.args_raw).ok();
                    let exited_zero = res.ui["exit"].as_i64() == Some(0) && res.ui["timedOut"] != true;
                    if exited_zero
                        && args
                            .as_ref()
                            .is_some_and(|args| crate::think::executed_check(&call.name, args).is_some())
                    {
                        self.proof.passed();
                    }
                }
                _ => {}
            }
        }
    }

    /// Start of a run: remember the tree, so a later `bash` change can be told apart.
    pub(crate) fn begin(&mut self, st: &SessionState) {
        if !st.settings.done_commands().is_empty() {
            self.tree = tree_fingerprint(&st.cwd);
        }
    }

    /// After a batch with any tool that may change files (`bash`, sub-agent, MCP and extension
    /// tools; not the read-only built-ins): a changed work folder counts as an edit.
    pub(crate) fn note_bash(&mut self, st: &SessionState, results: &[CallResult]) {
        const READ_ONLY: [&str; 4] = ["read", "grep", "find", "ls"];
        let file_edit = results
            .iter()
            .any(|r| !r.is_error && (r.tool_name == "edit" || r.tool_name == "write"));
        // Already dirty and unproven: a look at the tree cannot change the answer.
        if (self.dirty && !self.proof.checked)
            || self.tree.is_none()
            || results.iter().all(|r| READ_ONLY.contains(&r.tool_name.as_str()))
        {
            // File tools mark the proof directly, but their successful edit must also advance
            // the fingerprint. Otherwise a passing check in the next batch compares with the
            // pre-edit tree and incorrectly treats the already-observed edit as a later change.
            if file_edit && self.tree.is_some() {
                self.observe_tree(tree_fingerprint(&st.cwd), true);
            }
            return;
        }
        let now = tree_fingerprint(&st.cwd);
        self.observe_tree(now, file_edit);
    }
}

/// Run the checks when due. `Some(text)` = a failure to send to the model for another round.
pub(crate) fn check(
    st: &mut SessionState,
    state: &mut DoneState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Option<String> {
    state.observation = None;
    let cmds = st.settings.done_commands();
    if cmds.is_empty() || !state.dirty || st.shared.cancel.is_cancelled() {
        return None;
    }
    state.dirty = false;
    let _ = ui;
    let mut failed = String::new();
    let mut failed_checks = Vec::new();
    for cmd in &cmds {
        // A cancel stops the whole done phase: no further command, no notice, no repair text.
        if st.shared.cancel.is_cancelled() {
            return None;
        }
        let (ok, line, out) = run_one(st, cmd);
        if st.shared.cancel.is_cancelled() {
            return None;
        }
        let mut text = line.clone();
        if !out.is_empty() {
            text.push('\n');
            text.push_str(&out);
        }
        let _ = crate::agent_loop::persist(
            st,
            EntryBody::Shown {
                text: text.clone(),
                why: shown_why::NOTICE,
            },
            sink,
        );
        sink.emit(&AgentEvent::Notice {
            level: if ok { Level::Info } else { Level::Error },
            text: text.clone(),
        });
        if !ok {
            let (dir, command) = parse_entry(cmd);
            failed_checks.push((dir.unwrap_or("."), command));
            failed.push_str(&text);
            failed.push_str("\n\n");
        }
    }
    // A check may rewrite files (formatters, generators): that is not a model edit.
    if state.tree.is_some() {
        state.tree = tree_fingerprint(&st.cwd);
    }
    // What print mode reads at the end: the last check round failed and nothing fixed it.
    st.stats.done_failed = !failed.is_empty();
    if failed.is_empty() {
        state.observation = Some(CheckObservation::Passed);
        state.rounds = 0;
        state.proof.passed();
        return None;
    }
    state.rounds = state.rounds.saturating_add(1);
    let repair = (state.rounds <= MAX_ROUNDS).then(|| {
        format!(
            "The project's done checks failed. Fix the problems, then finish.\n\n{}",
            failed.trim_end()
        )
    });
    state.observation = Some(CheckObservation::Failed {
        identity: serde_json::json!(failed_checks).to_string(),
        rounds: state.rounds,
        // A newly delivered suggestion can support one repair after the normal rounds.
        // Subsequent checks can settle or escalate that advice without another call.
        evidence: (state.rounds <= MAX_ROUNDS + 1).then_some(failed),
    });
    repair
}

pub(crate) fn inject(
    st: &mut SessionState,
    text: String,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
    run_msgs: &mut Vec<EventMessage>,
) -> Result<(), OxaError> {
    crate::agent_loop::push_user(st, text, sink, ui, run_msgs)
}

/// A `done` entry: the command and the folder it runs in (as written), if the entry has the
/// `in <dir>: <command>` form. Anything else is a plain command.
pub(crate) fn parse_entry(entry: &str) -> (Option<&str>, &str) {
    let e = entry.trim();
    if let Some(rest) = e.strip_prefix("in ") {
        if let Some((dir, cmd)) = rest.split_once(':') {
            return (Some(dir.trim()), cmd.trim());
        }
    }
    (None, e)
}

/// `<dir>` of an `in <dir>: <command>` entry, relative to the canonical session folder
/// (empty/`.` = the folder itself). `..`, absolute paths and symlinks that leave the session
/// folder are refused. The `Err` is the reason.
fn resolve_dir(cwd: &Path, dir: &str) -> Result<PathBuf, String> {
    if dir.is_empty() {
        return Err("no folder after `in`".to_string());
    }
    let mut lexical = PathBuf::new();
    for c in cwd.join(dir).components() {
        match c {
            Component::ParentDir => {
                lexical.pop();
            }
            Component::CurDir => {}
            other => lexical.push(other.as_os_str()),
        }
    }
    let canon_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let outside = || format!("folder {dir} is outside the project folder");
    if !lexical.starts_with(cwd) && !lexical.starts_with(&canon_cwd) {
        return Err(outside());
    }
    match lexical.canonicalize() {
        Ok(real) if !real.starts_with(&canon_cwd) => Err(outside()),
        Ok(real) if !real.is_dir() => Err(format!("{dir} is not a folder")),
        Ok(real) => {
            let rel = real.strip_prefix(&canon_cwd).unwrap_or(Path::new("."));
            Ok(if rel.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                rel.to_path_buf()
            })
        }
        Err(_) => Err(format!("folder {dir} does not exist")),
    }
}

/// `s` as one POSIX shell word.
fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run one entry; returns (passed, header line, trimmed output).
fn run_one(st: &SessionState, entry: &str) -> (bool, String, String) {
    let (dir, cmd) = parse_entry(entry);
    let label = match dir {
        Some(d) => format!("[{d}] {cmd}"),
        None => cmd.to_string(),
    };
    // The sandbox authority always roots at the original workspace (`st.cwd`); a subdir entry
    // is selected inside that view with a shell `cd`, so a swapped symlink cannot move the
    // writable bind root to an outside directory.
    let child = match dir {
        None => None,
        Some(d) => match resolve_dir(&st.cwd, d) {
            Ok(p) => Some(format!(
                "cd -- {} || exit 111\n",
                single_quote(&p.to_string_lossy())
            )),
            Err(why) => return (false, format!("done check: {label} FAILED (not run)"), why),
        },
    };
    if cmd.is_empty() {
        return (
            false,
            format!("done check: {label} FAILED (not run)"),
            "no command after `:`".to_string(),
        );
    }
    let shell = oxa_tools::ShellConfig {
        shell_path: st.settings.shell_path(&st.dirs.home),
        command_prefix: st.settings.shell_command_prefix(),
    };
    // The `cd` goes before the fully resolved shell text, so shellCommandPrefix and a custom
    // shell both start in the child folder.
    let command = match shell.command(cmd) {
        Ok(c) => format!("{}{c}", child.as_deref().unwrap_or("")),
        Err(e) => return (false, format!("done check: {label} FAILED (not run)"), e),
    };
    let spec = ExecSpec {
        command,
        cwd: st.cwd.clone(),
        writable: true,
        net: st.sandbox.network(),
        timeout_s: Some(TIMEOUT_S),
        env: st.tool_env.clone(),
    };
    let res = st.sandbox.run(&spec, &st.shared.cancel, &mut |_| {});
    let out = tail(&strip_ansi(&String::from_utf8_lossy(&res.output)));
    if res.exit == Some(0) && !res.timed_out {
        return (true, format!("done check: {label} ok"), String::new());
    }
    let why = if res.timed_out {
        "timed out".to_string()
    } else {
        match res.exit {
            Some(n) => format!("exit {n}"),
            None => "killed".to_string(),
        }
    };
    (false, format!("done check: {label} FAILED ({why})"), out)
}

fn tail(s: &str) -> String {
    let s = s.trim_end();
    if s.len() <= OUT_CAP {
        return s.to_string();
    }
    let mut from = s.len() - OUT_CAP;
    while !s.is_char_boundary(from) {
        from += 1;
    }
    format!("[output truncated]\n{}", &s[from..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[cfg(target_os = "linux")]
    #[test]
    fn fingerprint_does_not_read_tracked_or_untracked_file_contents() {
        // Linux counts read(2) bytes even when they hit the page cache. Use this thread's
        // counter so concurrent tests and the git pipe-reader threads cannot affect it.
        fn bytes_read() -> u64 {
            std::fs::read_to_string("/proc/thread-self/io")
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("rchar: "))
                .unwrap()
                .parse()
                .unwrap()
        }

        let dir = std::env::temp_dir().join(format!("oxa-fp-no-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        const FILE_SIZE: usize = 64 * 1024;
        for name in ["tracked.txt", "untracked.txt"] {
            std::fs::write(dir.join(name), vec![b'x'; FILE_SIZE]).unwrap();
        }
        assert!(std::process::Command::new("git")
            .args(["add", "--", "tracked.txt"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());

        // Prove that the counter detects a real content read before testing the probe.
        let before = bytes_read();
        assert_eq!(std::fs::read(dir.join("tracked.txt")).unwrap().len(), FILE_SIZE);
        assert!(bytes_read() - before >= FILE_SIZE as u64);
        for age in [0, 3600] {
            let mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(age);
            for name in ["tracked.txt", "untracked.txt"] {
                std::fs::File::options()
                    .write(true)
                    .open(dir.join(name))
                    .unwrap()
                    .set_modified(mtime)
                    .unwrap();
            }
            let before = bytes_read();
            let fingerprint = tree_fingerprint(&dir);
            let read = bytes_read() - before;
            assert!(fingerprint.is_some());
            // Allow the tiny /proc accounting read itself, but not even one fixture file.
            assert!(read < FILE_SIZE as u64, "probe read {read} bytes at age {age}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fingerprint_stays_stable_as_an_unchanged_file_ages() {
        let dir = std::env::temp_dir().join(format!("oxa-fp-aging-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        std::fs::write(dir.join("a.txt"), "unchanged\n").unwrap();
        let before = tree_fingerprint(&dir).unwrap();
        // Cross the former two-second content-hashing window without changing the file.
        std::thread::sleep(std::time::Duration::from_secs(3));
        assert_eq!(before, tree_fingerprint(&dir).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fingerprint_sees_a_same_size_rewrite_with_restored_mtime() {
        let dir = std::env::temp_dir().join(format!("oxa-fp-rewrite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .unwrap()
            .success());
        let f = dir.join("a.txt");
        std::fs::write(&f, "before\n").unwrap();
        let file = std::fs::File::options().write(true).open(&f).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        file.set_modified(old).unwrap();
        let metadata = file.metadata().unwrap();
        let before = tree_fingerprint(&dir).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&f, "after!\n").unwrap();
        file.set_modified(old).unwrap();
        assert_eq!(metadata.len(), file.metadata().unwrap().len());
        assert_eq!(
            metadata.modified().unwrap(),
            file.metadata().unwrap().modified().unwrap()
        );
        assert_ne!(before, tree_fingerprint(&dir).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn fingerprint_sees_a_mode_change_of_an_old_untracked_file() {
        let dir = std::env::temp_dir().join(format!("oxa-fp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git_ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(git_ok, "git init");
        let f = dir.join("a.sh");
        std::fs::write(&f, "echo hi\n").unwrap();
        // Old mtime: the change below cannot be seen through contents or mtime.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let before = tree_fingerprint(&dir).unwrap();
        assert_eq!(before, tree_fingerprint(&dir).unwrap(), "stable without changes");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(before, tree_fingerprint(&dir).unwrap(), "chmod is a change");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn walk_entries_stops_enumerating_at_the_remaining_budget() {
        let mut seen = WALK_CAP - 2;
        let mut reads = 0;
        let entries = (0..10).map(|n| {
            reads += 1;
            Ok(n)
        });
        assert!(walk_entries(entries, &mut seen).is_none());
        assert_eq!(reads, 3, "stop before collecting or sorting the whole directory");
    }

    #[test]
    fn walk_entries_counts_errors_against_the_budget() {
        let mut seen = WALK_CAP - 1;
        let entries = [Ok(1), Err(std::io::Error::other("unreadable entry")), Ok(2)];
        assert!(walk_entries(entries.into_iter(), &mut seen).is_none());
        assert_eq!(seen, WALK_CAP + 1);
    }

    #[test]
    fn walk_entries_accepts_exactly_the_budget() {
        let mut seen = WALK_CAP - 2;
        assert_eq!(
            walk_entries([Ok(1), Ok(2)].into_iter(), &mut seen),
            Some(vec![1, 2])
        );
        assert_eq!(seen, WALK_CAP);
        assert_eq!(
            walk_entries(std::iter::empty::<std::io::Result<()>>(), &mut seen),
            Some(vec![])
        );
    }

    #[test]
    fn walk_fingerprint_detects_same_size_same_mtime_edits() {
        let env = oxa_fake::TestEnv::new();
        let path = env.work().join("existing.txt");
        std::fs::write(&path, "before").unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let before = walk_fingerprint(&env.work()).unwrap();
        assert_eq!(walk_fingerprint(&env.work()), Some(before));
        std::fs::write(&path, "after!").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), modified);
        assert_ne!(walk_fingerprint(&env.work()), Some(before));
    }

    #[test]
    fn edited_without_a_passing_check_is_unproven() {
        let mut proof = Proof::default();
        proof.changed();
        assert!(proof.unproven());
    }

    #[test]
    fn a_passing_check_after_the_last_edit_is_proven() {
        let mut proof = Proof::default();
        proof.changed();
        proof.passed();
        assert!(!proof.unproven());
        // A new edit after the check makes the run unproven again.
        proof.changed();
        assert!(proof.unproven());
    }

    #[test]
    fn a_check_before_the_last_edit_does_not_prove_it() {
        let mut proof = Proof::default();
        proof.changed();
        proof.passed();
        proof.changed();
        assert!(proof.unproven());
    }

    #[test]
    fn observed_edit_fingerprint_does_not_undo_a_later_passing_check() {
        let mut state = DoneState {
            tree: Some(1),
            ..DoneState::default()
        };
        state.dirty = true;
        state.proof.changed();

        // This is the file-edit branch of note_bash: synchronize the edit while it is known.
        state.observe_tree(Some(2), true);
        state.proof.passed();
        state.observe_tree(Some(2), false);

        assert!(!state.proof.unproven());
    }

    #[test]
    fn read_only_runs_are_never_asked() {
        let mut proof = Proof::default();
        assert!(!proof.unproven());
        proof.passed();
        assert!(!proof.unproven());
    }

    #[test]
    fn echoed_check_does_not_prove_an_edit() {
        let mut state = DoneState::default();
        state.proof.changed();
        let call = ToolCall {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            args_raw: serde_json::json!({ "command": "echo pytest" }).to_string(),
        };
        let result = CallResult {
            msg: oxa_proto::WireMsg::tool_result("call-1", "pytest"),
            tool_name: "bash".to_string(),
            is_error: false,
            ui: serde_json::json!({ "exit": 0, "timedOut": false }),
            warn: false,
        };

        state.note_proof(&[call], &[result]);

        assert!(state.proof.unproven());
    }
}
