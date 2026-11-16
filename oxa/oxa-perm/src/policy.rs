//! A10: the mode x access policy table.
use crate::paths::{is_inside, is_read_only_command, resolve_real};
use oxa_proto::guard::{
    home_protected_paths, is_home_or_above, sensitive_paths, GIT_PROTECTED_IN_CWD, PROTECTED_IN_CWD,
};
use oxa_proto::{Access, Decision, Mode, Policy, ToolClass, YoloSwitch};
use std::path::{Path, PathBuf};

/// The fixed `Decision::Deny` reasons (they become tool error text; W4).
pub mod deny_reasons {
    pub const PLAN_WRITE: &str = "plan mode does not allow writing files";
    pub const PLAN_EXEC: &str = "plan mode allows read-only commands only";
    pub const PLAN_NET: &str = "plan mode does not allow network access";
    pub const PLAN_MCP: &str = "plan mode allows read-only MCP tools only";
    /// B3: the deny-list (`oxa_proto::guard::sensitive_paths`).
    pub const SENSITIVE: &str = "this path holds keys, tokens or agent state; no tool may read or write it";
    /// B4: `.git/hooks`, `.git/config` (run by git outside the sandbox).
    pub const GIT_PROTECTED: &str =
        "git hooks and .git/config run outside the sandbox; the user edits them, not a tool";
}

/// B3 (wave H lane `harden`): the deny-list of the file tools, the same paths the bash sandbox
/// masks. `allow` holds the exceptions inside a denied path (codemode's shared spill directory,
/// `<tmp>/codemode`); `spill` is the current session's spill directory (`<tmp>/<session id>`),
/// the only other part of `tmp` the file tools reach: other sessions' spill files stay denied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathGuard {
    deny: Vec<PathBuf>,
    allow: Vec<PathBuf>,
    /// The current session's spill directory: the full output of a capped result is read from
    /// here without asking (#522).
    spill: Option<PathBuf>,
    /// Readable exceptions inside a denied path: the built-in skills the prompt advertises.
    read_allow: Vec<PathBuf>,
    /// The home directory and its startup files, keys and agent settings
    /// (`oxa_proto::guard::PROTECTED_IN_HOME`): a write asks when the cwd is the home
    /// directory or above it (O-199 first run: oxa started in `$HOME`).
    home: PathBuf,
    home_protected: Vec<PathBuf>,
}

impl PathGuard {
    /// The deny-list of `home` and `agent_dir` only (use [`PathGuard::with_real_home`] to add the
    /// account's real home), with codemode's spill directory under `tmp_dir` excepted. The
    /// session's own spill directory is set with [`PathGuard::set_spill_dir`].
    pub fn new(home: &Path, agent_dir: &Path, tmp_dir: &Path) -> PathGuard {
        PathGuard::with_real_home(home, None, agent_dir, tmp_dir)
    }

    /// Like [`PathGuard::new`], plus the deny-list of the account's real home.
    pub fn with_real_home(
        home: &Path,
        real_home: Option<&Path>,
        agent_dir: &Path,
        tmp_dir: &Path,
    ) -> PathGuard {
        // The same dual-home deny-list as the bash sandbox: a changed `$HOME` never widens access.
        let mut deny = sensitive_paths(home, Some(agent_dir));
        if let Some(r) = real_home.filter(|r| *r != home) {
            deny.extend(sensitive_paths(r, None));
        }
        deny.sort();
        deny.dedup();
        let mut home_protected = home_protected_paths(home);
        if let Some(r) = real_home.filter(|r| *r != home) {
            home_protected.extend(home_protected_paths(r));
        }
        PathGuard {
            deny,
            allow: vec![tmp_dir.join("codemode")],
            spill: None,
            read_allow: vec![agent_dir.join("builtin-skills")],
            home: home.to_path_buf(),
            home_protected,
        }
    }

    /// A write to `p` (absolute) that runs or configures something outside the sandbox later:
    /// with `cwd` the home directory or above it, and (#201) for every write outside the cwd
    /// wherever the cwd is, so a grant never covers these files.
    fn protects_home_file(&self, p: &Path, cwd: &Path) -> bool {
        !self.home_protected.is_empty()
            && self.home_protected.iter().any(|d| is_inside(p, d))
            && (!is_inside(p, cwd)
                || is_home_or_above(cwd, &self.home)
                || resolve_real(cwd).is_some_and(|c| is_home_or_above(&c, &self.home)))
    }

    /// Point the exception at the spill directory of the session now running.
    pub fn set_spill_dir(&mut self, dir: &Path) {
        self.spill = Some(dir.to_path_buf());
    }

    /// `p` (absolute) is inside the current session's spill directory.
    pub fn is_spill(&self, p: &Path) -> bool {
        self.spill.as_ref().is_some_and(|s| is_inside(p, s))
    }

    /// `p` (absolute) is on the deny-list, after resolving `..` and symlinks.
    pub fn denies(&self, p: &Path) -> bool {
        self.deny.iter().any(|d| is_inside(p, d))
            && !self.allow.iter().any(|a| is_inside(p, a))
            && !self.is_spill(p)
    }

    /// Like [`PathGuard::denies`] for a read: the built-in skills are readable.
    pub fn denies_read(&self, p: &Path) -> bool {
        self.denies(p) && !self.is_builtin_skill(p)
    }

    /// `p` (absolute) is inside the built-in skills directory the agent instructions advertise.
    pub fn is_builtin_skill(&self, p: &Path) -> bool {
        self.read_allow.iter().any(|a| is_inside(p, a))
    }
}

/// B4: how a write to `p` inside `cwd` is protected: `Some(true)` = never by a tool (git),
/// `Some(false)` = only after the user confirms, `None` = not protected.
fn protected_write(p: &Path, cwd: &Path) -> Option<bool> {
    // A worktree's `.git` is a file that names the git directory git runs hooks and config
    // from; it runs outside the sandbox later, like the files below.
    if p == cwd.join(".git") {
        return Some(true);
    }
    let hit = |list: &[&str]| list.iter().any(|rel| is_inside(p, &cwd.join(rel)));
    if hit(GIT_PROTECTED_IN_CWD) || in_linked_git_dirs(p, cwd) {
        Some(true)
    } else if hit(PROTECTED_IN_CWD) {
        Some(false)
    } else {
        None
    }
}

fn is_regular_file(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file())
}

/// A worktree's `.git` is a file pointing at its gitdir, whose `commondir` holds the real hooks
/// and config. `true` when `p` is a hooks dir or config file of either, outside the cwd.
fn in_linked_git_dirs(p: &Path, cwd: &Path) -> bool {
    let Some(start) = resolve_real(cwd) else {
        return false;
    };
    let Some(dotgit) = start.ancestors().map(|a| a.join(".git")).find(|d| d.exists()) else {
        return false;
    };
    // Only a regular file: a FIFO or device would block the read on every write check.
    if !is_regular_file(&dotgit) {
        return false; // a normal repository: the cwd table covers it
    }
    let Ok(text) = std::fs::read_to_string(&dotgit) else {
        return false;
    };
    let Some(raw) = text.trim().strip_prefix("gitdir:") else {
        return false;
    };
    let base = dotgit.parent().unwrap_or(cwd);
    let gitdir = base.join(raw.trim());
    // The pointer itself decides which dirs are protected, so rewriting it is never allowed.
    if is_inside(p, &dotgit) {
        return true;
    }
    let mut dirs = vec![gitdir.clone()];
    let common = gitdir.join("commondir");
    if is_regular_file(&common) {
        if let Ok(c) = std::fs::read_to_string(&common) {
            dirs.push(gitdir.join(c.trim()));
        }
    }
    dirs.iter().any(|d| {
        let d = resolve_real(d).unwrap_or_else(|| d.clone());
        ["hooks", "config", "config.worktree", "commondir", "gitdir"]
            .iter()
            .any(|rel| is_inside(p, &d.join(rel)))
    })
}

/// The policy of the crate docs' table, with per-session grants.
///
/// Grant encoding (what [`ModePolicy::grants`] returns):
/// * `Read(dir)` / `Write(dir)`: `dir` is the subtree the grant covers (the path's directory,
///   or the path itself when it is an existing directory), resolved.
/// * `Exec { command, net: true }`: that exact command line.
/// * `Exec { command: "", net: false }`: any command without network.
/// * `Net { host }`: the host.
/// * `Mcp { server, tool, class }`: matched by server and tool; `class` is informational.
#[derive(Debug, Clone, Default)]
pub struct ModePolicy {
    grants: Vec<Access>,
    guard: PathGuard,
    /// O-182: while on, a question of the table is answered with "allow" (see
    /// [`ModePolicy::with_yolo`]).
    yolo: Option<YoloSwitch>,
}

impl PartialEq for ModePolicy {
    fn eq(&self, other: &ModePolicy) -> bool {
        let same_switch = match (&self.yolo, &other.yolo) {
            (None, None) => true,
            (Some(a), Some(b)) => a.same(b),
            _ => false,
        };
        self.grants == other.grants && self.guard == other.guard && same_switch
    }
}

impl Eq for ModePolicy {}

impl ModePolicy {
    pub fn new() -> ModePolicy {
        ModePolicy::default()
    }
    /// B3: deny the paths of `guard` to the file tools, in every mode, whatever the grants.
    pub fn with_guard(mut self, guard: PathGuard) -> ModePolicy {
        self.guard = guard;
        self
    }
    /// O-182 (YOLO): while `switch` is on, every `Decision::Ask` of the mode table becomes
    /// `Decision::Allow` and is counted on the switch. The safety floor is not part of the
    /// table and is unchanged: the deny-list (B3) and git's files (B4) stay `Deny`, plan
    /// mode's `Deny`s stand, and a write to the project's protected configuration (`.oxa`,
    /// B4: it is loaded outside the sandbox later) still asks.
    pub fn with_yolo(mut self, switch: YoloSwitch) -> ModePolicy {
        self.yolo = Some(switch);
        self
    }
    pub fn grants(&self) -> &[Access] {
        &self.grants
    }

    /// `true` when a remembered grant covers `a`.
    fn granted(&self, a: &Access) -> bool {
        self.grants.iter().any(|g| match (g, a) {
            (Access::Read(dir), Access::Read(p)) | (Access::Write(dir), Access::Write(p)) => {
                is_inside(p, dir)
            }
            (
                Access::Exec {
                    command: gc,
                    net: true,
                },
                Access::Exec {
                    command: c,
                    net: true,
                },
            ) => gc == c,
            (Access::Exec { net: false, .. }, Access::Exec { net: false, .. }) => true,
            (Access::Net { host: gh }, Access::Net { host }) => gh == host,
            (
                Access::Mcp {
                    server: gs, tool: gt, ..
                },
                Access::Mcp { server, tool, .. },
            ) => gs == server && gt == tool,
            _ => false,
        })
    }
}

/// The directory a grant on `path` covers: the path when it is an existing directory, else its
/// parent. Resolved, so later containment checks compare real locations.
fn grant_scope(path: &Path) -> std::path::PathBuf {
    let real = resolve_real(path).unwrap_or_else(|| path.to_path_buf());
    if real.is_dir() {
        real
    } else {
        real.parent().map(|p| p.to_path_buf()).unwrap_or(real)
    }
}

/// The table of the crate docs, before grants.
fn table(mode: Mode, a: &Access, cwd: &Path) -> Decision {
    match a {
        Access::Read(p) => {
            if is_inside(p, cwd) {
                Decision::Allow
            } else {
                Decision::Ask
            }
        }
        Access::Write(p) => match mode {
            Mode::Plan => Decision::Deny(deny_reasons::PLAN_WRITE),
            Mode::Workspace => {
                if is_inside(p, cwd) {
                    Decision::Allow
                } else {
                    Decision::Ask
                }
            }
            Mode::Ask => Decision::Ask,
        },
        Access::Exec { command, net } => match (mode, *net) {
            (Mode::Plan, false) => {
                if is_read_only_command(command) {
                    Decision::Allow
                } else {
                    Decision::Deny(deny_reasons::PLAN_EXEC)
                }
            }
            (Mode::Plan, true) => Decision::Deny(deny_reasons::PLAN_NET),
            // Sandboxed: read-only or not, no prompt.
            (Mode::Workspace, false) => Decision::Allow,
            (Mode::Workspace, true) => Decision::Ask,
            (Mode::Ask, _) => Decision::Ask,
        },
        Access::Net { .. } => match mode {
            Mode::Plan => Decision::Deny(deny_reasons::PLAN_NET),
            _ => Decision::Ask,
        },
        Access::Mcp { class, .. } => match (mode, class) {
            (Mode::Plan, ToolClass::Read) | (Mode::Workspace, ToolClass::Read) => Decision::Allow,
            (Mode::Ask, ToolClass::Read) => Decision::Ask,
            (Mode::Plan, _) => Decision::Deny(deny_reasons::PLAN_MCP),
            _ => Decision::Ask,
        },
    }
}

impl Policy for ModePolicy {
    fn decide(&self, mode: Mode, a: &Access, cwd: &Path) -> Decision {
        if let Access::Read(p) | Access::Write(p) = a {
            let abs = crate::paths::normalize(p, cwd);
            // B3: the deny-list beats the mode, the cwd and every grant.
            let denied = match a {
                Access::Read(_) => self.guard.denies_read(&abs),
                _ => self.guard.denies(&abs),
            };
            if denied {
                return Decision::Deny(deny_reasons::SENSITIVE);
            }
            // #492: the advertised built-in skills are read without asking, wherever the cwd is.
            if matches!(a, Access::Read(_)) && self.guard.is_builtin_skill(&abs) {
                return Decision::Allow;
            }
            // #522: the full output of a capped result is read without asking, so an
            // unattended run can recover it.
            if matches!(a, Access::Read(_)) && self.guard.is_spill(&abs) {
                return Decision::Allow;
            }
            // B4: files that run outside the sandbox later: never (git) or ask, no grant.
            if let Access::Write(_) = a {
                let protected = protected_write(&abs, cwd)
                    .or_else(|| self.guard.protects_home_file(&abs, cwd).then_some(false));
                match (protected, mode) {
                    (Some(true), _) => return Decision::Deny(deny_reasons::GIT_PROTECTED),
                    (Some(false), Mode::Plan) => return Decision::Deny(deny_reasons::PLAN_WRITE),
                    (Some(false), _) => return Decision::Ask,
                    (None, _) => {}
                }
            }
        }
        // Grants only turn Ask into Allow; a Deny from the table always stands.
        match table(mode, a, cwd) {
            Decision::Ask if self.granted(a) => Decision::Allow,
            Decision::Ask => match &self.yolo {
                Some(switch) if switch.is_on() => {
                    switch.count();
                    Decision::Allow
                }
                _ => Decision::Ask,
            },
            d => d,
        }
    }

    fn set_spill_dir(&mut self, dir: &Path) {
        self.guard.set_spill_dir(dir);
    }

    fn remember(&mut self, a: &Access) {
        let grant = match a {
            Access::Read(p) => Access::Read(grant_scope(p)),
            Access::Write(p) => Access::Write(grant_scope(p)),
            Access::Exec { command, net: true } => Access::Exec {
                command: command.clone(),
                net: true,
            },
            Access::Exec { net: false, .. } => Access::Exec {
                command: String::new(),
                net: false,
            },
            Access::Net { host } => Access::Net { host: host.clone() },
            Access::Mcp { server, tool, class } => Access::Mcp {
                server: server.clone(),
                tool: tool.clone(),
                class: *class,
            },
        };
        if !self.grants.contains(&grant) {
            self.grants.push(grant);
        }
    }
}
