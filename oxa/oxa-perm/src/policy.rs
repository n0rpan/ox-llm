//! A10: the mode x access policy table.
use crate::danger::{has_dynamic, has_mutable_input, DangerHit, DangerRules};
use crate::desk::{ApprovalDesk, UnattendedMode};
use crate::paths::{is_inside, is_read_only_command, resolve_real};
use oxa_proto::guard::{
    home_protected_paths, is_home_or_above, sensitive_paths, GIT_PROTECTED_IN_CWD, PROTECTED_IN_CWD,
};
use oxa_proto::{tool_errors, Access, Decision, Mode, PathFilter, Policy, ToolClass, YoloSwitch};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
/// `<tmp>/codemode`); `spill` is the current session's spill directory (`<tmp>/<session id>`).
/// Fork/clone ancestors' spills are read-only exceptions; unrelated sessions stay denied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathGuard {
    deny: Vec<PathBuf>,
    allow: Vec<PathBuf>,
    /// The current session's spill directory: the full output of a capped result is read from
    /// here without asking (#522).
    spill: Option<PathBuf>,
    /// #597: the session's scratch directory, the host side of `/tmp` in the bash sandbox: what
    /// bash wrote there is read without asking.
    scratch: Option<PathBuf>,
    /// Source sessions of a fork/clone: copied history still points at these outputs.
    inherited_spills: Vec<PathBuf>,
    /// Readable exceptions inside a denied path: the built-in skills the prompt advertises.
    read_allow: Vec<PathBuf>,
    /// Exact Cargo configuration files already exposed read-only in the bash sandbox (#611).
    /// These bypass approval only, never the deny-list.
    cargo_configs: Vec<PathBuf>,
    /// Clipboard images use generated names directly under tmp, not a session directory.
    paste_dir: Option<PathBuf>,
    /// A pasted-image name cannot override a credential path that resolves to that image.
    paste_deny: Vec<PathBuf>,
    /// The home directory and its startup files, keys and agent settings
    /// (`oxa_proto::guard::PROTECTED_IN_HOME`): a write asks when the cwd is the home
    /// directory or above it (O-199 first run: oxa started in `$HOME`).
    /// Crate-private for the #1512 classifier: `~` and `$HOME` expand here.
    pub(crate) home: PathBuf,
    /// Crate-private for the #1512 classifier: writes here are oxa's own state.
    pub(crate) agent_dir: PathBuf,
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
        // Only the agent-state mask is excepted for images, never a credential mask. Keep
        // these paths separate so a credential symlink to a generated image stays denied.
        let mut paste_deny = sensitive_paths(home, None);
        paste_deny.retain(|p| p != &home.join(".oxa/agent"));
        if let Some(r) = real_home.filter(|r| *r != home) {
            paste_deny.extend(
                sensitive_paths(r, None)
                    .into_iter()
                    .filter(|p| p != &r.join(".oxa/agent")),
            );
        }
        let mut home_protected = home_protected_paths(home);
        if let Some(r) = real_home.filter(|r| *r != home) {
            home_protected.extend(home_protected_paths(r));
        }
        PathGuard {
            deny,
            allow: vec![tmp_dir.join("codemode")],
            spill: None,
            scratch: None,
            inherited_spills: Vec::new(),
            read_allow: vec![agent_dir.join("builtin-skills")],
            cargo_configs: std::iter::once(home)
                .chain(real_home)
                .flat_map(|h| [h.join(".cargo/config.toml"), h.join(".cargo/config")])
                .collect(),
            paste_dir: Some(tmp_dir.to_path_buf()),
            paste_deny,
            home: home.to_path_buf(),
            agent_dir: agent_dir.to_path_buf(),
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

    /// #597: read the session's scratch directory `dir` without asking. Reads only: it is not
    /// on the deny-list, so a link out of it is judged at its target like any other path.
    pub fn with_scratch(mut self, dir: &Path) -> PathGuard {
        self.scratch = Some(dir.to_path_buf());
        self
    }

    /// `p` (absolute) is inside the session's scratch directory.
    fn is_scratch(&self, p: &Path) -> bool {
        self.scratch.as_ref().is_some_and(|s| is_inside(p, s))
    }

    /// Point the exception at the spill directory of the session now running. Switching
    /// sessions revokes inherited exceptions until its ancestry is supplied again.
    pub fn set_spill_dir(&mut self, dir: &Path) {
        self.spill = Some(dir.to_path_buf());
        self.inherited_spills.clear();
    }

    /// `p` (absolute) is inside the current session's spill directory.
    pub fn is_spill(&self, p: &Path) -> bool {
        self.spill.as_ref().is_some_and(|s| is_inside(p, s))
    }

    /// `p` is output of this session or a fork/clone ancestor, not an unrelated session.
    fn is_readable_spill(&self, p: &Path) -> bool {
        self.is_spill(p) || self.inherited_spills.iter().any(|s| is_inside(p, s))
    }

    /// `p` (absolute) is on the deny-list, after resolving `..` and symlinks.
    pub fn denies(&self, p: &Path) -> bool {
        self.deny.iter().any(|d| is_inside(p, d))
            && !self.allow.iter().any(|a| is_inside(p, a))
            && !self.is_spill(p)
    }

    /// `dir` (absolute) holds a deny-list entry or the agent directory: removing it recursively
    /// takes a credential or agent path along.
    pub fn denies_within(&self, dir: &Path) -> bool {
        self.deny.iter().any(|d| is_inside(d, dir))
            || (!self.agent_dir.as_os_str().is_empty() && is_inside(&self.agent_dir, dir))
    }

    /// Like [`PathGuard::denies`] for a read, with skills, inherited output and pasted images
    /// excepted. Images still go through the ordinary outside-project approval table.
    pub fn denies_read(&self, p: &Path) -> bool {
        let pasted = self
            .paste_dir
            .as_ref()
            .is_some_and(|dir| match (resolve_real(p), resolve_real(dir)) {
                (Some(real), Some(root)) => {
                    is_pasted_image(&real, &root) && !self.paste_deny.iter().any(|d| is_inside(&real, d))
                }
                _ => false,
            });
        self.denies(p) && !self.is_builtin_skill(p) && !self.is_readable_spill(p) && !pasted
    }

    /// `p` (absolute) is inside the built-in skills directory the agent instructions advertise.
    pub fn is_builtin_skill(&self, p: &Path) -> bool {
        self.read_allow.iter().any(|a| is_inside(p, a))
    }

    /// An exact Cargo config, not its directory or siblings. Resolve both sides so symlinked
    /// homes/configs work; the caller must check the deny-list first, including the real path.
    fn is_cargo_config(&self, p: &Path) -> bool {
        resolve_real(p).is_some_and(|real| {
            self.cargo_configs
                .iter()
                .any(|config| resolve_real(config).as_ref() == Some(&real))
        })
    }

    /// #517: [`PathGuard::denies_read`] for the entries of a directory walk, with every list
    /// resolved once here instead of once per entry.
    pub fn hidden_paths(&self) -> HiddenPaths {
        // Both forms: the written one and the real one (a symlinked home, a link to a key dir).
        let both = |list: &[PathBuf]| -> Vec<PathBuf> {
            let mut out: Vec<PathBuf> = list
                .iter()
                .flat_map(|p| [Some(p.clone()), resolve_real(p)])
                .flatten()
                .collect();
            out.sort();
            out.dedup();
            out
        };
        let mut allow = both(&self.allow);
        allow.extend(both(&self.read_allow));
        allow.extend(both(&self.spill.iter().cloned().collect::<Vec<_>>()));
        allow.extend(both(&self.inherited_spills));
        HiddenPaths {
            deny: both(&self.deny),
            allow,
            paste_dirs: both(&self.paste_dir.iter().cloned().collect::<Vec<_>>()),
            paste_deny: both(&self.paste_deny),
        }
    }
}

/// Match only clipboard.rs's `paste-<16 lowercase hex digits>.<image extension>` directly
/// under the resolved tmp root. Callers resolve symlinks before matching, so an image name
/// cannot expose a key or another session's output through a link.
fn is_pasted_image(real: &Path, tmp: &Path) -> bool {
    if real.parent() != Some(tmp) {
        return false;
    }
    let Some(name) = real.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some((hash, ext)) = name.strip_prefix("paste-").and_then(|n| n.split_once('.')) else {
        return false;
    };
    hash.len() == 16
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && matches!(ext, "png" | "jpg" | "gif" | "webp")
}

/// #517: the deny-list of a [`PathGuard`] as a [`PathFilter`] for walked entries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HiddenPaths {
    deny: Vec<PathBuf>,
    /// The readable exceptions inside a denied path (spill files, built-in skills).
    allow: Vec<PathBuf>,
    /// Resolved roots for generated clipboard images, including ones pasted after this snapshot.
    paste_dirs: Vec<PathBuf>,
    /// Credential masks still apply even to a file with a generated image name.
    paste_deny: Vec<PathBuf>,
}

impl PathFilter for HiddenPaths {
    fn hides(&self, real: &Path) -> bool {
        self.deny.iter().any(|d| real.starts_with(d))
            && !self.allow.iter().any(|a| real.starts_with(a))
            && (!self.paste_dirs.iter().any(|d| is_pasted_image(real, d))
                || self.paste_deny.iter().any(|d| real.starts_with(d)))
    }
}

/// B4: how a write to `p` inside `cwd` is protected: `Some(true)` = never by a tool (git),
/// `Some(false)` = only after the user confirms, `None` = not protected.
pub(crate) fn protected_write(p: &Path, cwd: &Path) -> Option<bool> {
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
    /// #517: `guard` for the file tools' directory walks (`None` without a guard).
    hidden: Option<Arc<HiddenPaths>>,
    /// O-182: while on, a question of the table is answered with "allow" (see
    /// [`ModePolicy::with_yolo`]).
    yolo: Option<YoloSwitch>,
    /// #1512 S2: the dangerous-command rules (`None` = no floor prompt).
    danger: Option<Arc<DangerRules>>,
    /// #1512 S2: the session's dangerous-command grants (see [`danger_key`]).
    danger_grants: Vec<String>,
    /// #1512 S2: the keys saved by "always allow" in the user settings.
    always: Arc<Vec<String>>,
    /// #1512 S5: the approval desk of an unattended run (`None` = no desk).
    desk: Option<Arc<ApprovalDesk>>,
    /// #1512 S5: the session the desk files requests and grants for.
    session_id: String,
    /// #1512 S5: how an unattended run answers a dangerous command (`oxa.approval.unattended`).
    unattended: UnattendedMode,
    /// #1512 S5: this run has the OS sandbox (`oxa.sandbox.unattended`, S3).
    os_sandbox: bool,
}

impl PartialEq for ModePolicy {
    fn eq(&self, other: &ModePolicy) -> bool {
        let same_switch = match (&self.yolo, &other.yolo) {
            (None, None) => true,
            (Some(a), Some(b)) => a.same(b),
            _ => false,
        };
        self.grants == other.grants
            && self.guard == other.guard
            && same_switch
            && self.danger == other.danger
            && self.danger_grants == other.danger_grants
            && self.always == other.always
            && self.desk == other.desk
            && self.session_id == other.session_id
            && self.unattended == other.unattended
            && self.os_sandbox == other.os_sandbox
    }
}

impl Eq for ModePolicy {}

impl ModePolicy {
    pub fn new() -> ModePolicy {
        ModePolicy::default()
    }
    /// B3: deny the paths of `guard` to the file tools, whatever the grants, except for
    /// user-selected full-access `yolo` outside plan mode.
    pub fn with_guard(mut self, guard: PathGuard) -> ModePolicy {
        self.hidden = Some(Arc::new(guard.hidden_paths()));
        self.guard = guard;
        self
    }
    /// O-182, #202: at `Autonomy::Auto` every `Decision::Ask` of the mode table becomes
    /// `Decision::Allow` and is counted on the switch; the safety floor is unchanged: the
    /// deny-list (B3) and git's files (B4) stay `Deny`, plan mode's `Deny`s stand, and a write
    /// to the project's protected configuration (`.oxa`) still asks. At `Autonomy::Yolo` (full
    /// access) every `Ask` and every `Deny` becomes `Allow` outside plan mode (owner's decision
    /// on #202/#314); only questions count as auto-approved, not bypassed denials (#443).
    /// In plan mode it answers as `Auto`.
    pub fn with_yolo(mut self, switch: YoloSwitch) -> ModePolicy {
        self.yolo = Some(switch);
        self
    }
    /// #1512 S2: dangerous commands ask. `rules` decide what is dangerous (see
    /// [`DangerRules::classify`]); `always` holds the keys saved by "always allow" (the user
    /// settings' `oxa.approval.always`), which are never asked again.
    pub fn with_danger(mut self, rules: Arc<DangerRules>, always: Vec<String>) -> ModePolicy {
        self.danger = Some(rules);
        self.always = Arc::new(always);
        self
    }
    /// #1512 S5: an unattended run files its dangerous commands at `desk` for `session_id`.
    /// `unattended` decides refuse or request (`Auto` = refuse under the OS sandbox, request
    /// without it); `os_sandbox` is this run's OS sandbox (S3).
    pub fn with_desk(
        mut self,
        desk: Arc<ApprovalDesk>,
        session_id: String,
        unattended: UnattendedMode,
        os_sandbox: bool,
    ) -> ModePolicy {
        self.desk = Some(desk);
        self.session_id = session_id;
        self.unattended = unattended;
        self.os_sandbox = os_sandbox;
        self
    }
    pub fn grants(&self) -> &[Access] {
        &self.grants
    }

    /// #1512 S2: the dangerous-command hits of `a` that no session or always grant covers, and
    /// that the desk's grants do not cover either. Only an exec is classified; the generic grants
    /// never cover these hits. Nothing is spent here: the caller may still turn this into a
    /// prompt, so a `once` grant is claimed only at the final policy decision.
    fn danger_hits(&self, a: &Access, cwd: &Path) -> Vec<DangerHit> {
        let open = self.danger_open(a, cwd);
        if self.desk_grant(a, cwd, &open).is_some() {
            Vec::new()
        } else {
            open
        }
    }

    /// The classified hits of `a` minus the keys a session or always grant covers. Inspect
    /// the whole command for dynamic input because an earlier stage can affect a later hit.
    fn danger_open(&self, a: &Access, cwd: &Path) -> Vec<DangerHit> {
        let (Access::Exec { command, .. }, Some(rules)) = (a, &self.danger) else {
            return Vec::new();
        };
        let mut found = rules.classify(command, cwd, &self.guard);
        // A segment a built-in rule already names (a force push) is asked and granted under that
        // rule's key alone; the publish rule adds the rest.
        for hit in rules.publish_classify(command, cwd) {
            if !found.iter().any(|h| h.segment == hit.segment) {
                found.push(hit);
            }
        }
        if has_dynamic(command) {
            return found;
        }
        found
            .into_iter()
            .filter(|h| {
                let key = danger_key(h, command);
                !self.danger_grants.contains(&key) && !self.always.contains(&key)
            })
            .collect()
    }

    /// #1512 S5: the desk's session or once grant covers `open` together, all or nothing. The
    /// key is the complete command line, exactly like the policy grants: an approval for
    /// `true || git push --force origin main` never covers a standalone push. Shell expansion
    /// markers anywhere in the command make a saved grant inapplicable, and so does input the
    /// command reads from a file or stdin (`xargs rm -rf < targets`): the file can change after
    /// the answer while the text stays the same.
    fn desk_grant(&self, a: &Access, cwd: &Path, open: &[DangerHit]) -> Option<(&ApprovalDesk, Vec<String>)> {
        let (Access::Exec { command, .. }, Some(desk)) = (a, &self.desk) else {
            return None;
        };
        if open.is_empty()
            || has_dynamic(command)
            || has_mutable_input(command)
            || open.iter().any(|h| h.rule == crate::desk::OPAQUE_SHELL_INPUT)
        {
            return None;
        }
        let mut keys: Vec<String> = open.iter().map(|h| danger_key(h, command)).collect();
        // A push is granted for the facts it acts on (source ref ids, remote identity), read
        // now, immediately before the run: the same text after a commit, a reset or a changed
        // remote is a new question (#2055). Facts that cannot be resolved grant nothing.
        match push_facts_of(command, cwd) {
            None => {}
            Some(Ok(digest)) => keys.push(crate::desk::facts_key(&digest)),
            Some(Err(())) => return None,
        }
        desk.covers(&self.session_id, cwd, &keys)
            .then_some((desk.as_ref(), keys))
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

/// A verdict before autonomy is applied: `AskAlways` is the safety floor's prompt, which
/// `auto` never turns into an allow. Only user-selected `yolo` outside plan mode bypasses it.
#[derive(Clone, Copy)]
enum Verdict {
    Is(Decision),
    AskAlways,
}

impl ModePolicy {
    /// The verdict for `a` with the grants, before YOLO.
    fn verdict(&self, mode: Mode, a: &Access, cwd: &Path) -> Verdict {
        if let Access::Read(p) | Access::Write(p) = a {
            let abs = crate::paths::normalize(p, cwd);
            // B3: the deny-list beats the mode, the cwd and every grant.
            let denied = match a {
                Access::Read(_) => self.guard.denies_read(&abs),
                _ => self.guard.denies(&abs),
            };
            if denied {
                return Verdict::Is(Decision::Deny(deny_reasons::SENSITIVE));
            }
            // #492, #522, #531, #597, #611: advertised built-in skills, this session's
            // output (including fork/clone ancestry), scratch directory and Cargo configs
            // are read without asking. The deny-list above still applies.
            if matches!(a, Access::Read(_))
                && (self.guard.is_builtin_skill(&abs)
                    || self.guard.is_readable_spill(&abs)
                    || self.guard.is_scratch(&abs)
                    || self.guard.is_cargo_config(&abs))
            {
                return Verdict::Is(Decision::Allow);
            }
            // B4: files that run outside the sandbox later: never (git) or ask, no grant.
            if let Access::Write(_) = a {
                let protected = protected_write(&abs, cwd)
                    .or_else(|| self.guard.protects_home_file(&abs, cwd).then_some(false));
                match (protected, mode) {
                    (Some(true), _) => return Verdict::Is(Decision::Deny(deny_reasons::GIT_PROTECTED)),
                    (Some(false), Mode::Plan) => {
                        return Verdict::Is(Decision::Deny(deny_reasons::PLAN_WRITE))
                    }
                    (Some(false), _) => return Verdict::AskAlways,
                    (None, _) => {}
                }
            }
        }
        // Grants only turn Ask into Allow; a Deny from the table always stands.
        Verdict::Is(match table(mode, a, cwd) {
            Decision::Ask if self.granted(a) => Decision::Allow,
            d => d,
        })
    }
}

/// The stricter of two verdicts: a `Deny` (the first one's reason first), the floor's prompt,
/// a prompt, then `Allow`.
fn stricter(a: Verdict, b: Verdict) -> Verdict {
    use Verdict::{AskAlways, Is};
    match (a, b) {
        (Is(Decision::Deny(r)), _) | (_, Is(Decision::Deny(r))) => Is(Decision::Deny(r)),
        (AskAlways, _) | (_, AskAlways) => AskAlways,
        (Is(Decision::Ask), _) | (_, Is(Decision::Ask)) => Is(Decision::Ask),
        _ => Is(Decision::Allow),
    }
}

impl Policy for ModePolicy {
    fn decide(&self, mode: Mode, a: &Access, cwd: &Path) -> Decision {
        self.decide_pinned(mode, a, cwd).0
    }

    /// #518: the path is resolved once; the verdict covers both the path as named and that real
    /// location (the stricter one wins), and the real location is what the tool may open.
    fn decide_pinned(&self, mode: Mode, a: &Access, cwd: &Path) -> (Decision, Option<PathBuf>) {
        let (mut verdict, pin) = match a {
            Access::Read(p) | Access::Write(p) => {
                let abs = crate::paths::normalize(p, cwd);
                // A path that does not resolve (a link loop) pins its lexical form: the open
                // then fails or lands elsewhere and is refused.
                let real = resolve_real(&abs).unwrap_or(abs);
                let at_real = match a {
                    Access::Read(_) => Access::Read(real.clone()),
                    _ => Access::Write(real.clone()),
                };
                let d = stricter(self.verdict(mode, a, cwd), self.verdict(mode, &at_real, cwd));
                (d, Some(real))
            }
            _ => (self.verdict(mode, a, cwd), None),
        };
        // The danger floor applies to execs the table would consider. Keep the grant used
        // for this decision so another retry cannot make it disappear before consumption.
        let mut desk_grant = None;
        if !matches!(verdict, Verdict::Is(Decision::Deny(_))) {
            let open = self.danger_open(a, cwd);
            desk_grant = self.desk_grant(a, cwd, &open);
            if !open.is_empty() && desk_grant.is_none() {
                verdict = stricter(verdict, Verdict::AskAlways);
            }
        }
        // Owner decision (#202/#314): yolo outside plan mode lifts every file-tool
        // guard. Resolve and pin first, so the open still uses the checked location.
        let full = self.yolo.as_ref().filter(|s| s.full() && mode != Mode::Plan);
        let decision = match verdict {
            Verdict::Is(Decision::Deny(_)) if full.is_some() => Decision::Allow,
            Verdict::AskAlways if full.is_some() => {
                // #443: only questions count as auto-approved; bypassed denials do not.
                full.unwrap().count();
                Decision::Allow
            }
            Verdict::AskAlways => Decision::Ask,
            Verdict::Is(Decision::Ask) => match &self.yolo {
                Some(switch) if switch.is_on() => {
                    switch.count();
                    Decision::Allow
                }
                _ => Decision::Ask,
            },
            Verdict::Is(d) => d,
        };
        // #1512 S5: a `once` grant is spent here and nowhere else: the decision above is final
        // and an `Allow` means this is the command that runs. A probe (the verdict, a
        // `danger_ask` the caller may turn into a prompt) leaves it available.
        if let Some((desk, keys)) = desk_grant.filter(|_| decision == Decision::Allow) {
            if !desk.granted(&self.session_id, cwd, &keys) {
                return (Decision::Deny("the command's approval could not be claimed"), pin);
            }
        }
        (decision, pin)
    }

    fn danger_ask(&self, a: &Access, cwd: &Path) -> Vec<(String, String)> {
        let Access::Exec { command, .. } = a else {
            return Vec::new();
        };
        self.danger_hits(a, cwd)
            .into_iter()
            .map(|h| {
                let reason = format!("{}: {}", h.rule, h.reason);
                (danger_key(&h, command), reason)
            })
            .collect()
    }

    fn grant_danger(&mut self, keys: &[String]) {
        for key in keys {
            if !self.danger_grants.contains(key) {
                self.danger_grants.push(key.clone());
            }
        }
    }

    fn set_session_id(&mut self, id: &str) {
        self.session_id = id.to_string();
    }

    /// #1512 S5: an exec that needs an answer in a run with no one to ask. With no desk, `None`
    /// (the caller asks the `Ui`, which refuses). Otherwise the refusal text, or the request
    /// text naming the filed request id.
    fn danger_unattended(&mut self, a: &Access, cwd: &Path) -> Option<String> {
        let Access::Exec { command, .. } = a else {
            return None;
        };
        let desk = self.desk.clone()?;
        let hits = self.danger_hits(a, cwd);
        if hits.is_empty() {
            return None;
        }
        let facts = push_facts_of(command, cwd);
        if hits.iter().any(|hit| hit.rule == crate::desk::OPAQUE_SHELL_INPUT)
            || has_mutable_input(command)
            || facts == Some(Err(()))
        {
            return Some(
                "Refused: shell input cannot be verified for unattended replay. Ask the owner to review and run it."
                    .into(),
            );
        }
        let why = hits
            .iter()
            .map(|h| format!("{}: {}", h.rule, h.reason))
            .collect::<Vec<_>>()
            .join("; ");
        let refuse = self.unattended.resolve(self.os_sandbox) == UnattendedMode::Refuse;
        // The OS-sandbox text only where the OS sandbox is the reason; a `refuse` setting
        // without it, or a request that could not be filed, says so truthfully.
        let text = if refuse && self.os_sandbox {
            tool_errors::DANGEROUS_REFUSED.replace("{why}", &why)
        } else if refuse {
            tool_errors::DANGEROUS_REFUSED_SETTING.replace("{why}", &why)
        } else {
            let digest = facts.and_then(Result::ok);
            match desk.record_bound(
                &self.session_id,
                cwd,
                command,
                &hits,
                "pending",
                digest.as_deref(),
            ) {
                Ok(id) => tool_errors::DANGEROUS_REQUESTED
                    .replace("{id}", &id)
                    .replace("{session}", &self.session_id)
                    .replace("{why}", &why),
                Err(_) => tool_errors::DANGEROUS_REFUSED_NO_REQUEST.replace("{why}", &why),
            }
        };
        Some(text)
    }

    fn set_spill_dir(&mut self, dir: &Path) {
        self.guard.set_spill_dir(dir);
        self.hidden = Some(Arc::new(self.guard.hidden_paths()));
    }

    fn set_inherited_spill_dirs(&mut self, dirs: &[PathBuf]) {
        self.guard.inherited_spills = dirs.to_vec();
        self.hidden = Some(Arc::new(self.guard.hidden_paths()));
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

    fn path_filter(&self) -> Option<Arc<dyn PathFilter>> {
        self.path_filter_for_mode(Mode::Workspace)
    }

    fn path_filter_for_mode(&self, mode: Mode) -> Option<Arc<dyn PathFilter>> {
        if mode != Mode::Plan && self.yolo.as_ref().is_some_and(YoloSwitch::full) {
            return None;
        }
        self.hidden.clone().map(|h| h as Arc<dyn PathFilter>)
    }
}

/// The resolved facts of the pushes in `command` (see `publish::push_facts`); `None` when it
/// pushes nothing. Text the tokenizer cannot read is unresolvable.
fn push_facts_of(command: &str, cwd: &Path) -> Option<Result<String, ()>> {
    match crate::danger::parse_line(command) {
        Some(stages) => crate::publish::push_facts(&stages, cwd),
        None => None,
    }
}

/// The key a dangerous-command grant stores and `oxa.approval.always` matches: the rule with the
/// whole command line it was found in. A hit's own segment is not enough, because a pipeline or
/// a dead branch repeats it (`curl a | bash` and `curl b | bash` both name `bash`; `true || git
/// push --force origin main` names the push), so approving one text would cover the other.
pub(crate) fn danger_key(h: &DangerHit, command: &str) -> String {
    // Trailing whitespace can be semantic shell syntax (an escaped space or backslash-newline),
    // so preserve the reviewed command byte-for-byte.
    format!("{} {command}", h.rule)
}
