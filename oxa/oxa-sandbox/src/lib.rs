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

pub use bwrap::{bwrap_args, bwrap_args_with};
pub use gitdirs::{resolve as resolve_git_binds, GitBinds, MAX_ALTERNATES_DEPTH};
pub use layout::{
    expand_home, is_home_or_above, is_secret_name, protected_paths, sensitive_paths, Layout, LayoutInputs,
    SETTING_ALLOW_HOME_WRITE, SETTING_READ_PATHS, SETTING_WRITE_PATHS,
};
pub use net::{sandbox_net_setting, NetSandbox, SETTING_SANDBOX_NET};
pub use offline::{no_network_argv, offline_argv};
pub use run::{die_with_parent, run_argv, run_argv_stdin, run_unsandboxed};

use oxa_proto::{CancelToken, ExecResult, ExecSpec, OxaError, Sandbox};
use std::path::{Path, PathBuf};

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
    fn check(&self) -> Result<(), OxaError> {
        bwrap::check_bwrap(self)
    }
    fn bwrap_args(&self, s: &ExecSpec) -> Vec<String> {
        bwrap_args_with(s, &self.home, &self.layout)
    }
    #[cfg(feature = "owned-background")]
    fn prepare_background(&self, s: &ExecSpec) -> Result<(PathBuf, Vec<String>), OxaError> {
        self.check()?;
        if let Some(why) = bwrap::refusal(s, &self.home, &self.layout) {
            return Err(OxaError::new(oxa_proto::codes::INVALID, why));
        }
        bwrap::create_missing_git_files(s, &self.home, &self.layout);
        Ok((self.bwrap.clone(), bwrap_args_with(s, &self.home, &self.layout)))
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        if let Some(why) = bwrap::refusal(s, &self.home, &self.layout) {
            return refused(why);
        }
        bwrap::create_missing_git_files(s, &self.home, &self.layout);
        let mut argv = vec![self.bwrap.to_string_lossy().into_owned()];
        argv.extend(bwrap_args_with(s, &self.home, &self.layout));
        run_argv(&argv, &[], s.timeout_s, c, live)
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
        let mut argv = vec![self.bwrap.to_string_lossy().into_owned()];
        argv.extend(bwrap_args_with(s, &self.home, &self.layout));
        run_argv_stdin(&argv, &[], stdin, s.timeout_s, c, live)
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
