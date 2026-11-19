//! The session probe (animations design, context layer, slice X2). Public API only: a fake
//! filesystem, no clock, no terminal.
use std::cell::RefCell;

use oxa_interactive::skin::stage::probe::{
    run_probe, CiProvider, HostFacts, Probe, ProbeFs, SessionProbe, READ_CAP, READ_PATHS, STAT_PATHS,
};

/// A fake filesystem: `present` lists the paths that exist, `contents` the readable files. Every
/// call is logged so a test can check which paths the probe touched.
#[derive(Default)]
struct FakeFs {
    present: Vec<&'static str>,
    contents: Vec<(&'static str, &'static str)>,
    stats: RefCell<Vec<String>>,
    reads: RefCell<Vec<String>>,
}

impl FakeFs {
    fn with(present: &[&'static str], contents: &[(&'static str, &'static str)]) -> Self {
        FakeFs {
            present: present.to_vec(),
            contents: contents.to_vec(),
            ..Default::default()
        }
    }

    fn touched(&self) -> usize {
        self.stats.borrow().len() + self.reads.borrow().len()
    }
}

impl ProbeFs for FakeFs {
    fn exists(&self, path: &str) -> bool {
        self.stats.borrow_mut().push(path.to_string());
        self.present.iter().any(|p| *p == path)
    }

    fn read_capped(&self, path: &str, max: usize) -> Option<String> {
        assert!(max <= READ_CAP, "a read asked for more than the cap");
        self.reads.borrow_mut().push(path.to_string());
        self.contents
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, text)| text.to_string())
    }
}

fn no_env(_: &str) -> Option<String> {
    None
}

fn env_with(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    }
}

#[test]
fn probe_reads_at_most_the_listed_paths() {
    let fs = FakeFs::with(
        &[".github/workflows", ".git/MERGE_HEAD"],
        &[
            (".git/HEAD", "ref: refs/heads/main\n"),
            ("/etc/os-release", "ID=omarchy\nID_LIKE=arch\n"),
            ("/sys/class/power_supply/BAT0/capacity", "80\n"),
        ],
    );
    run_probe(&fs, &no_env);
    let stats = fs.stats.borrow();
    let reads = fs.reads.borrow();
    assert!(stats.len() <= 12, "too many existence checks: {}", stats.len());
    assert!(reads.len() <= 5, "too many reads: {}", reads.len());
    for path in stats.iter() {
        assert!(
            STAT_PATHS.iter().any(|listed| *listed == path.as_str())
                || path.ends_with("/MERGE_HEAD")
                || path.ends_with("/rebase-merge"),
            "unlisted existence check: {path}"
        );
    }
    for path in reads.iter() {
        assert!(
            READ_PATHS.iter().any(|listed| *listed == path.as_str()) || path.ends_with("/HEAD"),
            "unlisted read: {path}"
        );
    }
}

#[test]
fn probe_runs_once_per_session() {
    let fs = FakeFs::with(&["Jenkinsfile"], &[]);
    let mut session = SessionProbe::default();
    let first = session.get(&fs, &no_env).clone();
    let touched = fs.touched();
    let second = session.get(&fs, &no_env).clone();
    assert_eq!(first, second);
    assert_eq!(fs.touched(), touched, "a second call must not touch the disk");
    assert_eq!(session.runs(), 1);
    assert_eq!(first.ci, Some(CiProvider::Jenkins));
}

#[test]
fn github_workflows_dir_is_ci_github() {
    let fs = FakeFs::with(&[".github/workflows"], &[]);
    let probe = run_probe(&fs, &no_env);
    assert_eq!(probe.ci, Some(CiProvider::Github));
    assert_eq!(CiProvider::Github.key(), "github");
}

#[test]
fn missing_files_are_not_errors() {
    let fs = FakeFs::default();
    assert_eq!(run_probe(&fs, &no_env), Probe::default());
}

#[test]
fn main_branch_is_default_and_a_commit_id_is_detached() {
    let on_main = FakeFs::with(&[], &[(".git/HEAD", "ref: refs/heads/main\n")]);
    let main = run_probe(&on_main, &no_env);
    assert!(main.on_default_branch);
    assert!(!main.detached);

    let detached = FakeFs::with(&[], &[(".git/HEAD", "0123abcd\n")]);
    let probe = run_probe(&detached, &no_env);
    assert!(!probe.on_default_branch);
    assert!(probe.detached);
}

#[test]
fn merge_and_rebase_are_read_from_the_git_dir() {
    let fs = FakeFs::with(&[".git/MERGE_HEAD", ".git/rebase-merge"], &[]);
    let probe = run_probe(&fs, &no_env);
    assert!(probe.merge_in_progress);
    assert!(probe.rebase_in_progress);
}

#[test]
fn git_file_points_to_the_per_worktree_git_dir() {
    let fs = FakeFs::with(
        &[
            "../repo/.git/worktrees/linked/MERGE_HEAD",
            "../repo/.git/worktrees/linked/rebase-merge",
        ],
        &[
            (".git", "gitdir: ../repo/.git/worktrees/linked\n"),
            ("../repo/.git/worktrees/linked/HEAD", "0123abcd\n"),
        ],
    );

    let probe = run_probe(&fs, &no_env);

    assert!(probe.detached);
    assert!(probe.merge_in_progress);
    assert!(probe.rebase_in_progress);
}

#[test]
fn host_and_battery_come_from_env_and_system_files() {
    let fs = FakeFs::with(
        &[],
        &[
            ("/etc/os-release", "NAME=\"Omarchy\"\nID=omarchy\nID_LIKE=arch\n"),
            ("/sys/class/power_supply/BAT0/capacity", "42\n"),
        ],
    );
    let env = env_with(&[
        ("SSH_CONNECTION", "192.0.2.10 50000 192.0.2.20 22"),
        ("TMUX", "socket,1,0"),
    ]);
    let probe = run_probe(&fs, &env);
    assert_eq!(
        probe.host,
        HostFacts {
            ssh: true,
            tmux: true,
            arch: true,
            omarchy: true,
        }
    );
    assert_eq!(probe.battery_percent, Some(42));
}

#[cfg(unix)]
#[test]
fn disk_probe_rejects_fifo_head_without_blocking() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::mpsc;
    use std::time::Duration;

    use oxa_interactive::skin::stage::probe::DiskFs;

    let root = std::env::temp_dir().join(format!("oxa-stage-probe-fifo-{}", std::process::id()));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let fifo = root.join("fifo");
    let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    std::os::unix::fs::symlink(&fifo, root.join(".git/HEAD")).unwrap();

    let (tx, rx) = mpsc::channel();
    let probe_root = root.clone();
    std::thread::spawn(move || {
        let result = run_probe(&DiskFs { root: probe_root }, &no_env);
        tx.send(result).ok();
    });
    let result = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("probing a FIFO through a symlink must not block");
    assert!(!result.on_default_branch);
    assert!(!result.detached);

    std::fs::remove_dir_all(root).unwrap();
}
