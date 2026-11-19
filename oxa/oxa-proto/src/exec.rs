use crate::{CancelToken, Mode, OxaError, ToolClass};
use std::path::{Path, PathBuf};

/// Quote a value as one POSIX shell word, including empty strings.
pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// One thing a tool, extension host function or MCP server wants to do. `Policy` decides on it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Access {
    /// Absolute, lexically normalised path.
    Read(PathBuf),
    /// Absolute, lexically normalised path.
    Write(PathBuf),
    Exec {
        command: String,
        net: bool,
    },
    Net {
        host: String,
    },
    Mcp {
        server: String,
        tool: String,
        class: ToolClass,
    },
}

/// The policy's verdict. `Deny` carries a fixed reason that becomes the tool error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(&'static str),
    Ask,
}

/// The user's reply to a permission prompt. `Session` grants are remembered per session only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Once,
    Session,
    Deny,
}

/// One sandboxed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecSpec {
    /// Run as `bash -c <command>`.
    pub command: String,
    pub cwd: PathBuf,
    /// `false` = cwd is mounted read-only too (plan mode).
    pub writable: bool,
    /// `false` = no network namespace access.
    pub net: bool,
    pub timeout_s: Option<u32>,
    /// Extra environment, applied after the sandbox's fixed base environment.
    pub env: Vec<(String, String)>,
}

/// Result of a sandboxed command. `output` is stdout and stderr interleaved as produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecResult {
    pub output: Vec<u8>,
    /// `None` when killed by a signal, a timeout or a cancel.
    pub exit: Option<i32>,
    pub timed_out: bool,
}

/// The process sandbox (bwrap). Implemented by `oxa-sandbox`; tools reach it through
/// [`ToolCtx`](crate::ToolCtx).
///
/// Contract: `--unshare-all --die-with-parent --new-session --cap-drop ALL`, `/` read-only, home
/// hidden except cwd, private `/tmp`, no network unless `ExecSpec::net`. No bwrap = `check`
/// fails with `no_sandbox` and bash refuses; there is no unsandboxed fallback.
pub trait Sandbox: Send + Sync {
    /// `Ok` when commands can be sandboxed on this machine.
    fn check(&self) -> Result<(), OxaError>;
    /// O-167(b): whether the bash tool's commands get the network (`ExecSpec::net`). The
    /// default is `false` (a sandbox that knows nothing of the setting keeps the old rule);
    /// `oxa_sandbox::NetSandbox` answers from `oxa.sandbox.net` and the offline switch.
    fn network(&self) -> bool {
        false
    }
    /// Persistence limits to include in human/model-facing tool results, not process bytes.
    fn persistence_notice(&self) -> Option<&str> {
        None
    }
    /// #526: the home directory this sandbox hides (not the process's `$HOME`), so tools
    /// need not read the process environment. `None` (the default) = unknown.
    fn home(&self) -> Option<&std::path::Path> {
        None
    }
    /// #597: the host directory this session's sandbox mounts at `/tmp`, if it has one. The file
    /// tools read a path under `/tmp` there, as the bash tool sees it. Default: none.
    fn scratch_dir(&self) -> Option<&std::path::Path> {
        None
    }
    /// The scratch backing an absolute `/tmp` name in this mode's active filesystem view.
    /// Full access uses host paths in workspace mode, but plan still uses sandbox scratch.
    fn file_scratch_dir(&self, _mode: Mode) -> Option<&Path> {
        self.scratch_dir()
    }
    /// Paths deliberately named on the host (explicit mounts, git data and agent outputs)
    /// must not be reinterpreted as session `/tmp`. This is path identity, not a permission
    /// grant: the file tools still check the resulting host path with the policy.
    fn preserves_host_path(&self, _path: &Path) -> bool {
        false
    }
    /// #457: the sandbox for one new or reopened session: the same machine view, with the
    /// state that belongs to a single session (its scratch directory at `/tmp`) of its own,
    /// released when the session drops it. `None` (the default) = share `self` unchanged.
    fn for_session(&self) -> Option<std::sync::Arc<dyn Sandbox>> {
        None
    }
    /// The exact `bwrap` argv (without the leading `bwrap`) for `s`. Pure; golden-tested.
    fn bwrap_args(&self, s: &ExecSpec) -> Vec<String>;
    /// Run `s`, streaming output chunks to `live`. Cancel and timeout kill the process group.
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult;
    /// Like [`Sandbox::run`], but the chunks go only to `live`; `ExecResult.output` is empty
    /// (for long-lived callers that keep their own bounded buffer). The default calls `run`
    /// and clears the output; real executors override it so nothing accumulates.
    fn run_streaming(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        let mut r = self.run(s, c, live);
        r.output.clear();
        r
    }
    /// [`Sandbox::run`] with `stdin` written to the command's standard input, which is then
    /// closed (E-INT: exec-hook payloads that do not fit a command-line argument). The default
    /// is for sandboxes that cannot feed stdin: nothing runs and the output says so
    /// (`exit: None`). `oxa-sandbox`'s `Bwrap` implements it.
    fn run_with_stdin(
        &self,
        _s: &ExecSpec,
        _stdin: &[u8],
        _c: &CancelToken,
        _live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        ExecResult {
            output: b"this sandbox cannot pass standard input to a command".to_vec(),
            exit: None,
            timed_out: false,
        }
    }
}

/// How much the policy decides on its own (#202). `Standard` asks the user what the mode table
/// asks; `Auto` answers those questions with "allow" while the sandbox, the deny-list and the
/// protected paths stay on; `Yolo` is full access: no prompts, bash runs without the sandbox in
/// the user's own environment; the file tools have no deny-list or protected paths.
/// Plan mode stays read-only under every level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Autonomy {
    #[default]
    Standard,
    Auto,
    Yolo,
}

impl Autonomy {
    pub fn as_str(self) -> &'static str {
        match self {
            Autonomy::Standard => "standard",
            Autonomy::Auto => "auto",
            Autonomy::Yolo => "yolo",
        }
    }
    fn from_u8(v: u8) -> Autonomy {
        match v {
            1 => Autonomy::Auto,
            2 => Autonomy::Yolo,
            _ => Autonomy::Standard,
        }
    }
    fn to_u8(self) -> u8 {
        match self {
            Autonomy::Standard => 0,
            Autonomy::Auto => 1,
            Autonomy::Yolo => 2,
        }
    }
}

/// The permission modes the user picks (#202): `standard` (prompts), `auto` (no prompts,
/// sandbox and deny-list on; the default of a fresh install), `yolo` (full access) and `plan`
/// (read-only). `permissions.defaultMode` holds one of these names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Standard,
    Auto,
    Yolo,
    Plan,
}

impl PermissionMode {
    pub const ALL: [PermissionMode; 4] = [
        PermissionMode::Standard,
        PermissionMode::Auto,
        PermissionMode::Yolo,
        PermissionMode::Plan,
    ];
    /// The mode of a fresh install.
    pub const DEFAULT: PermissionMode = PermissionMode::Auto;
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Standard => "standard",
            PermissionMode::Auto => "auto",
            PermissionMode::Yolo => "yolo",
            PermissionMode::Plan => "plan",
        }
    }
    pub fn parse(s: &str) -> Option<PermissionMode> {
        Self::ALL.into_iter().find(|m| m.as_str() == s.trim())
    }
    /// How much the mode allows: plan < standard < auto < yolo. A project's settings may only
    /// pick a mode whose rank is not above the user's.
    pub fn rank(self) -> u8 {
        match self {
            PermissionMode::Plan => 0,
            PermissionMode::Standard => 1,
            PermissionMode::Auto => 2,
            PermissionMode::Yolo => 3,
        }
    }
    /// The autonomy level of the mode (plan keeps prompts as they are: it denies itself).
    pub fn autonomy(self) -> Autonomy {
        match self {
            PermissionMode::Auto => Autonomy::Auto,
            PermissionMode::Yolo => Autonomy::Yolo,
            PermissionMode::Standard | PermissionMode::Plan => Autonomy::Standard,
        }
    }
    /// The mode shown for a session's table mode and the process's autonomy.
    pub fn of(mode: Mode, autonomy: Autonomy) -> PermissionMode {
        match (mode, autonomy) {
            (Mode::Plan, _) => PermissionMode::Plan,
            (_, Autonomy::Standard) => PermissionMode::Standard,
            (_, Autonomy::Auto) => PermissionMode::Auto,
            (_, Autonomy::Yolo) => PermissionMode::Yolo,
        }
    }
    /// The mode key's cycle: standard -> auto -> yolo -> standard (plan -> standard).
    pub fn next(self) -> PermissionMode {
        match self {
            PermissionMode::Standard => PermissionMode::Auto,
            PermissionMode::Auto => PermissionMode::Yolo,
            PermissionMode::Yolo | PermissionMode::Plan => PermissionMode::Standard,
        }
    }
    /// One line on what the mode does (start notices, stderr in print/rpc runs, the picker).
    pub fn describe(self) -> &'static str {
        self.describe_with(OS_SANDBOX)
    }
    /// [`describe`](Self::describe) for a run with or without the OS sandbox. Without one the
    /// text never says the bash sandbox is on: it says there is none and that dangerous commands
    /// ask (#1512 S3: interactive runs are unsandboxed by default and the floor prompt stays).
    pub fn describe_with(self, os_sandbox: bool) -> &'static str {
        match (self, os_sandbox) {
            (PermissionMode::Standard, true) => {
                "prompts before actions outside the workspace; sandbox and deny-list on"
            }
            (PermissionMode::Standard, false) => {
                "prompts before actions outside the workspace; no OS sandbox, dangerous commands \
                 ask; deny-list on"
            }
            (PermissionMode::Auto, true) => {
                "no prompts; the bash sandbox, the deny-list and the protected paths stay on"
            }
            (PermissionMode::Auto, false) => {
                "no prompts; no OS sandbox, dangerous commands still ask; the deny-list and \
                 protected paths stay on"
            }
            (PermissionMode::Yolo, _) => {
                "full access: no prompts, bash runs without the sandbox in your own environment, \
                 no deny-list and no protected paths for the file tools"
            }
            (PermissionMode::Plan, _) => "read-only: plans, writes nothing",
        }
    }
}

/// Whether this build has an OS sandbox for bash (bubblewrap, Linux only). Off Linux nothing
/// sandboxes commands: `NoSandbox` refuses to run them unless yolo, and no text may claim
/// otherwise.
pub const OS_SANDBOX: bool = cfg!(target_os = "linux");

/// The start notice for a platform without an OS sandbox; `None` where there is one.
pub fn no_os_sandbox_notice() -> Option<&'static str> {
    if OS_SANDBOX {
        None
    } else if cfg!(target_os = "macos") {
        Some("macOS: no OS sandbox; bash is disabled unless yolo")
    } else {
        Some("no OS sandbox on this platform; bash is disabled unless yolo")
    }
}

/// The autonomy switch of a process (O-182, #202): [`Autonomy`] as one shared value. `Auto`
/// answers the mode table's questions with "allow" and counts them; floor prompts still ask.
/// At `Yolo` outside plan mode the file tools also allow floor prompts and denials, and bash
/// runs without the sandbox. Only questions count, not bypassed denials (#443). Cloning shares
/// the switch (the policy of every session, the bash tool and the UI hold the same one).
#[derive(Debug, Clone, Default)]
pub struct YoloSwitch {
    inner: std::sync::Arc<YoloInner>,
}

#[derive(Debug, Default)]
struct YoloInner {
    level: std::sync::atomic::AtomicU8,
    approved: std::sync::atomic::AtomicU64,
}

impl YoloSwitch {
    pub fn new(level: Autonomy) -> YoloSwitch {
        let switch = YoloSwitch::default();
        switch.set_level(level);
        switch
    }
    pub fn level(&self) -> Autonomy {
        Autonomy::from_u8(self.inner.level.load(std::sync::atomic::Ordering::SeqCst))
    }
    pub fn set_level(&self, level: Autonomy) {
        self.inner
            .level
            .store(level.to_u8(), std::sync::atomic::Ordering::SeqCst);
    }
    /// No prompts (`auto` or `yolo`).
    pub fn is_on(&self) -> bool {
        self.level() != Autonomy::Standard
    }
    /// Full access (`yolo`).
    pub fn full(&self) -> bool {
        self.level() == Autonomy::Yolo
    }
    /// How many questions were answered with "allow" since the process started.
    pub fn approved(&self) -> u64 {
        self.inner.approved.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// Count one auto-approved question.
    pub fn count(&self) {
        self.inner
            .approved
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    /// `true` when both are the same switch.
    pub fn same(&self, other: &YoloSwitch) -> bool {
        std::sync::Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// The permission policy: a pure table over (mode, access, cwd) plus per-session grants.
pub trait Policy: Send {
    fn decide(&self, mode: Mode, a: &Access, cwd: &Path) -> Decision;
    /// #518: [`Policy::decide`], plus, for an [`Access::Read`] or [`Access::Write`], the real
    /// location (symlinks resolved) the decision covers. The caller hands it to the tool
    /// (`ToolCtx::checked`), which then opens that location and nothing else, so a symlink
    /// swapped between the check and the open cannot redirect it. The default pins nothing.
    fn decide_pinned(&self, mode: Mode, a: &Access, cwd: &Path) -> (Decision, Option<PathBuf>) {
        (self.decide(mode, a, cwd), None)
    }
    /// Record an [`Answer::Session`] grant for `a`. Grants never outlive the session.
    fn remember(&mut self, a: &Access);
    /// The spill directory of the session that now runs: the full output of a capped result
    /// lives there, and a read of it needs no question. Default: nothing special.
    fn set_spill_dir(&mut self, _dir: &Path) {}
    /// Read-only spill directories inherited through the session's fork/clone ancestry.
    /// Replaces the previous list; unrelated sessions must not be included.
    fn set_inherited_spill_dirs(&mut self, _dirs: &[PathBuf]) {}
    /// #517: the deny-list for the entries a tool finds by walking an allowed directory
    /// ([`crate::ToolCtx::hidden`]). `None` (the default) = the policy hides nothing.
    fn path_filter(&self) -> Option<std::sync::Arc<dyn crate::PathFilter>> {
        None
    }
    /// The walk filter for the active table mode. Full access may lift the filter outside
    /// plan mode, while plan mode must keep the floor. Other policies retain their filter.
    fn path_filter_for_mode(&self, _mode: Mode) -> Option<std::sync::Arc<dyn crate::PathFilter>> {
        self.path_filter()
    }
    /// #1512 S2: the dangerous-command floor for `a` (an exec of `cwd`): one `(key, "rule: reason")`
    /// per matching rule that no grant covers. Empty = no dangerous prompt. The key is what a
    /// session or always grant stores ([`Policy::grant_danger`]).
    fn danger_ask(&self, _a: &Access, _cwd: &Path) -> Vec<(String, String)> {
        Vec::new()
    }
    /// #1512 S2: remember the keys of [`Policy::danger_ask`] for this session (`Session` and
    /// `Always` answers). Never covers any other access.
    fn grant_danger(&mut self, _keys: &[String]) {}
}
