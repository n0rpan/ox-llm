//! `done` setting (issue #104): shell commands oxa runs itself before it reports completion.
//! A run that edited files (`edit` / `write` succeeded, or a `bash` call changed the git tree) and is
//! about to stop runs them in the session sandbox; a failure goes back to the model as a user
//! message, at most [`MAX_ROUNDS`] times. An entry `in <dir>: <command>` runs in `<dir>`, a folder
//! of the session folder, instead of in the session folder itself.
use crate::exec::CallResult;
use crate::SessionState;
use oxa_proto::{shown_why, AgentEvent, EntryBody, EventMessage, EventSink, ExecSpec, Level, OxaError, Ui};
use oxa_tools::strip_ansi;
use std::path::{Component, Path, PathBuf};

/// Automatic fix rounds after a failed check; the next failure ends the run.
const MAX_ROUNDS: u32 = 2;
/// Seconds one command may run.
const TIMEOUT_S: u32 = 600;
/// Bytes of output kept per command (the tail).
const OUT_CAP: usize = 4096;

#[derive(Default)]
pub(crate) struct DoneState {
    /// A file tool succeeded since the last check.
    dirty: bool,
    rounds: u32,
    /// `git` fingerprint of the work folder at the last look (`None`: not a git repo).
    tree: Option<u64>,
}

/// Seconds a git probe may run before the fingerprint gives up (`None`: checks stay as they were).
const PROBE_TIMEOUT_S: u64 = 20;

/// Hash of the work folder's git file list (index entries and untracked files) with each file's
/// size, times, inode and mode; `None` outside a git repo, without git, or on a timeout.
/// The probes run on the host, so they only list files (`ls-files`): nothing that can start a
/// repository-configured program (`status` and `diff` run clean filters, textconv, external
/// diff). Files are stat-ed here instead of diffed by git.
fn tree_fingerprint(cwd: &Path) -> Option<u64> {
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
    pub(crate) fn note_edits(&mut self, results: &[CallResult]) {
        if results
            .iter()
            .any(|r| !r.is_error && (r.tool_name == "edit" || r.tool_name == "write"))
        {
            self.dirty = true;
        }
    }

    /// Start of a run: remember the tree, so a later `bash` change can be told apart.
    pub(crate) fn begin(&mut self, st: &SessionState) {
        if !st.settings.done_commands().is_empty() {
            self.tree = tree_fingerprint(&st.cwd);
        }
    }

    /// After a batch with a `bash` call: a changed git tree counts as an edit.
    pub(crate) fn note_bash(&mut self, st: &SessionState, results: &[CallResult]) {
        if self.tree.is_none() || !results.iter().any(|r| r.tool_name == "bash") {
            return;
        }
        let now = tree_fingerprint(&st.cwd);
        if now != self.tree {
            self.dirty = true;
            self.tree = now;
        }
    }
}

/// Run the checks when due. `Some(text)` = a failure to send to the model for another round.
pub(crate) fn check(
    st: &mut SessionState,
    state: &mut DoneState,
    sink: &mut dyn EventSink,
    ui: &mut dyn Ui,
) -> Option<String> {
    let cmds = st.settings.done_commands();
    if cmds.is_empty() || !state.dirty || st.shared.cancel.is_cancelled() {
        return None;
    }
    state.dirty = false;
    let _ = ui;
    let mut failed = String::new();
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
        state.rounds = 0;
        return None;
    }
    if state.rounds >= MAX_ROUNDS {
        return None;
    }
    state.rounds += 1;
    Some(format!(
        "The project's done checks failed. Fix the problems, then finish.\n\n{}",
        failed.trim_end()
    ))
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
    use super::tree_fingerprint;
    use std::os::unix::fs::PermissionsExt;

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
}
