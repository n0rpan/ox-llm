//! The session probe (animations design, context layer): the facts about the repo and the host
//! that the signals need, read once per session and never in the draw path.
//!
//! The reads are bounded: a fixed list of existence checks ([`STAT_PATHS`]) and a fixed list of
//! small reads ([`READ_PATHS`]), each read capped at [`READ_CAP`] bytes. A missing file or
//! variable is a fact ("no CI file", "not a repository"), never an error. Text from the repo is
//! reduced to flags and classes on the way in: no branch name or file content is kept.
//! Nothing here reads a clock.

use std::io::Read;
use std::path::PathBuf;

/// Existence checks the probe makes. Every CI marker and the git state files are listed here.
pub const STAT_PATHS: &[&str] = &[
    ".github/workflows",
    ".gitlab-ci.yml",
    ".circleci/config.yml",
    "Jenkinsfile",
    ".woodpecker.yml",
    ".buildkite/pipeline.yml",
    ".git/MERGE_HEAD",
    ".git/rebase-merge",
];

/// Small reads the probe makes: the HEAD ref, the battery level and the OS release.
pub const READ_PATHS: &[&str] = &[
    ".git/HEAD",
    "/sys/class/power_supply/BAT0/capacity",
    "/etc/os-release",
];

/// The most bytes one read takes.
pub const READ_CAP: usize = 4096;

const BATTERY_PATH: &str = "/sys/class/power_supply/BAT0/capacity";

/// The CI markers, in the order they are checked. The first one that exists names the provider.
const CI_MARKERS: &[(&str, CiProvider)] = &[
    (".github/workflows", CiProvider::Github),
    (".gitlab-ci.yml", CiProvider::Gitlab),
    (".circleci/config.yml", CiProvider::Circle),
    ("Jenkinsfile", CiProvider::Jenkins),
    (".woodpecker.yml", CiProvider::Woodpecker),
    (".buildkite/pipeline.yml", CiProvider::Buildkite),
];

/// A CI provider, named by the file that marks it. [`CiProvider::key`] is the signal value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiProvider {
    Github,
    Gitlab,
    Circle,
    Jenkins,
    Woodpecker,
    Buildkite,
}

impl CiProvider {
    /// The signal value of this provider.
    pub fn key(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Circle => "circle",
            Self::Jenkins => "jenkins",
            Self::Woodpecker => "woodpecker",
            Self::Buildkite => "buildkite",
        }
    }
}

/// Host facts for the `host` signal. `offline` and `yolo` come from settings, not from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostFacts {
    /// Running over SSH (`SSH_CONNECTION` or `SSH_TTY` is set).
    pub ssh: bool,
    /// Running inside tmux (`TMUX` is set).
    pub tmux: bool,
    /// An Arch system, by `ID=arch` or `arch` in `ID_LIKE`.
    pub arch: bool,
    /// Omarchy, by `ID=omarchy`.
    pub omarchy: bool,
}

/// The facts of one session, read once at start.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Probe {
    /// The CI provider, if a CI marker exists in the project.
    pub ci: Option<CiProvider>,
    pub host: HostFacts,
    /// HEAD is `main` or `master`.
    pub on_default_branch: bool,
    /// HEAD holds a commit id, not a branch.
    pub detached: bool,
    /// `.git/MERGE_HEAD` exists: a merge is in progress.
    pub merge_in_progress: bool,
    /// `.git/rebase-merge` exists: a rebase is in progress.
    pub rebase_in_progress: bool,
    /// The battery level, 0 to 100, when the machine has a battery.
    pub battery_percent: Option<u8>,
}

/// The filesystem the probe reads, injected so tests can use a fake.
pub trait ProbeFs {
    /// Whether a file or a directory exists at `path`.
    fn exists(&self, path: &str) -> bool;
    /// The first `max` bytes of the file at `path`, or `None` when it cannot be read.
    fn read_capped(&self, path: &str, max: usize) -> Option<String>;
}

/// The real filesystem. Relative paths resolve under `root`; absolute paths are used as they are.
pub struct DiskFs {
    pub root: PathBuf,
}

impl ProbeFs for DiskFs {
    fn exists(&self, path: &str) -> bool {
        self.root.join(path).exists()
    }

    fn read_capped(&self, path: &str, max: usize) -> Option<String> {
        let file = std::fs::File::open(self.root.join(path)).ok()?;
        let mut buf = Vec::new();
        file.take(max as u64).read_to_end(&mut buf).ok()?;
        Some(String::from_utf8_lossy(&buf).into_owned())
    }
}

/// Reads the probe once. `env` looks up one environment variable by name.
pub fn run_probe(fs: &dyn ProbeFs, env: &dyn Fn(&str) -> Option<String>) -> Probe {
    let ci = CI_MARKERS
        .iter()
        .find(|(path, _)| fs.exists(*path))
        .map(|(_, ci)| *ci);
    let head = fs.read_capped(".git/HEAD", READ_CAP);
    let (on_default_branch, detached) = head_state(head.as_deref());
    let os_release = fs.read_capped("/etc/os-release", READ_CAP);
    let (arch, omarchy) = os_release_facts(os_release.as_deref());
    let host = HostFacts {
        ssh: is_set(env("SSH_CONNECTION")) || is_set(env("SSH_TTY")),
        tmux: is_set(env("TMUX")),
        arch,
        omarchy,
    };
    let battery = fs.read_capped(BATTERY_PATH, READ_CAP);
    Probe {
        ci,
        host,
        on_default_branch,
        detached,
        merge_in_progress: fs.exists(".git/MERGE_HEAD"),
        rebase_in_progress: fs.exists(".git/rebase-merge"),
        battery_percent: battery.as_deref().and_then(parse_percent),
    }
}

/// The probe of one session. The first call reads the disk; later calls return the same value
/// without touching the disk again.
#[derive(Debug, Default)]
pub struct SessionProbe {
    cached: Option<Probe>,
    runs: u32,
}

impl SessionProbe {
    /// The session's probe, read on the first call.
    pub fn get(&mut self, fs: &dyn ProbeFs, env: &dyn Fn(&str) -> Option<String>) -> &Probe {
        let runs = &mut self.runs;
        self.cached.get_or_insert_with(|| {
            *runs += 1;
            run_probe(fs, env)
        })
    }

    /// How many times the disk was read for this session (0 or 1).
    pub fn runs(&self) -> u32 {
        self.runs
    }
}

fn is_set(value: Option<String>) -> bool {
    value.is_some_and(|v| !v.trim().is_empty())
}

/// Branch and detached state from `.git/HEAD`: `ref: refs/heads/<name>`, or a bare commit id.
/// Returns `(on_default_branch, detached)`.
fn head_state(head: Option<&str>) -> (bool, bool) {
    let Some(head) = head.map(str::trim) else {
        return (false, false);
    };
    match head.strip_prefix("ref:") {
        Some(target) => {
            let name = target.trim().strip_prefix("refs/heads/").unwrap_or("");
            (matches!(name, "main" | "master"), false)
        }
        None => (false, !head.is_empty()),
    }
}

/// `(arch, omarchy)` from `/etc/os-release`: `ID` names the distribution, `ID_LIKE` its parents.
fn os_release_facts(text: Option<&str>) -> (bool, bool) {
    let Some(text) = text else {
        return (false, false);
    };
    let mut id = "";
    let mut like = "";
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("ID=") {
            id = unquote(value);
        } else if let Some(value) = line.strip_prefix("ID_LIKE=") {
            like = unquote(value);
        }
    }
    let arch = id == "arch" || like.split_whitespace().any(|word| word == "arch");
    (arch, id == "omarchy")
}

fn unquote(value: &str) -> &str {
    value.trim().trim_matches('"').trim_matches('\'')
}

/// A battery level from its sysfs text: a whole number from 0 to 100.
fn parse_percent(text: &str) -> Option<u8> {
    text.trim().parse::<u8>().ok().filter(|p| *p <= 100)
}
