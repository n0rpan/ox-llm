use crate::{CancelToken, Mode, OxaError, ToolClass};
use std::path::{Path, PathBuf};

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
    /// The exact `bwrap` argv (without the leading `bwrap`) for `s`. Pure; golden-tested.
    fn bwrap_args(&self, s: &ExecSpec) -> Vec<String>;
    /// Run `s`, streaming output chunks to `live`. Cancel and timeout kill the process group.
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult;
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

/// The YOLO switch of a process (O-182): while it is on, the policy answers every question
/// it would have asked the user with "allow" and counts it. It removes prompts only: a
/// `Decision::Deny` (the deny-list of keys and tokens, git hooks, plan mode) stands, writes to
/// the project's `.oxa` configuration still ask, and the sandbox is not touched by it at all.
/// Cloning shares the switch (the policy of every session and the UI hold the same one).
#[derive(Debug, Clone, Default)]
pub struct YoloSwitch {
    inner: std::sync::Arc<YoloInner>,
}

#[derive(Debug, Default)]
struct YoloInner {
    on: std::sync::atomic::AtomicBool,
    approved: std::sync::atomic::AtomicU64,
}

impl YoloSwitch {
    pub fn new(on: bool) -> YoloSwitch {
        let switch = YoloSwitch::default();
        switch.set(on);
        switch
    }
    pub fn is_on(&self) -> bool {
        self.inner.on.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub fn set(&self, on: bool) {
        self.inner.on.store(on, std::sync::atomic::Ordering::SeqCst);
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
    /// Record an [`Answer::Session`] grant for `a`. Grants never outlive the session.
    fn remember(&mut self, a: &Access);
}
