//! #202: full access (`yolo`). The bash tool's commands run without bwrap, as `bash -c` with
//! the user's own rights and environment, while the process's [`YoloSwitch`] is at `yolo`.
//! A read-only spec (plan mode) and every other level go to the wrapped sandbox unchanged.
//! Offline stays a hard switch: the command then runs through bwrap with the network unshared
//! and the user's file system (as `!cmd` does offline), and is refused without bwrap.
//!
//! #1512 S3: without the OS sandbox (`os_sandbox: false`, the interactive default on Linux)
//! bash runs with the user's rights too; the approval floor of the policy
//! asks about dangerous commands. The owner's `oxa.sandbox.net = off` then holds: such a
//! command goes through bwrap without network, and is refused without bwrap.
use crate::{
    run_unsandboxed_capture, run_unsandboxed_stdin, ExecDecision, ExecPolicy, ExecRole, Layout, NetSandbox,
};
use oxa_proto::{CancelToken, ExecResult, ExecSpec, OxaError, Sandbox, YoloSwitch};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The bash tool's sandbox with the full-access bypass of `yolo`. Where a command runs is one
/// [`ExecDecision`] of the run's [`ExecPolicy`] (`decision.rs`); this type only carries it out.
pub struct FullAccess {
    inner: Arc<dyn Sandbox>,
    switch: YoloSwitch,
    home: PathBuf,
    policy: ExecPolicy,
    /// #1512 S3: the OS sandbox is part of this run's policy (default on).
    os_sandbox: Arc<AtomicBool>,
    host_layout: Layout,
    host_net: Option<Arc<NetSandbox>>,
}

impl FullAccess {
    /// `path_var` and `home` locate bwrap for a network-less host run; `offline` is the hard
    /// switch; `net_off` is the owner's `oxa.sandbox.net = off`. The policy is an unattended run
    /// with the OS sandbox on; [`FullAccess::with_policy`] replaces it.
    pub fn new(
        inner: Arc<dyn Sandbox>,
        switch: YoloSwitch,
        path_var: &str,
        home: PathBuf,
        offline: bool,
        net_off: bool,
    ) -> FullAccess {
        let policy = ExecPolicy::new(path_var, &home, offline, net_off);
        FullAccess::with_policy(inner, switch, home, policy)
    }

    /// A full-access sandbox for the run's execution policy, computed once at startup (the
    /// pinned bwrap is found there, not here).
    pub fn with_policy(
        inner: Arc<dyn Sandbox>,
        switch: YoloSwitch,
        home: PathBuf,
        policy: ExecPolicy,
    ) -> FullAccess {
        FullAccess {
            inner,
            switch,
            home,
            policy,
            os_sandbox: Arc::new(AtomicBool::new(true)),
            host_layout: Layout::default(),
            host_net: None,
        }
    }

    /// #1512 S3: `false` runs model bash with the user's rights (no OS sandbox).
    pub fn with_os_sandbox(mut self, on: bool) -> FullAccess {
        self.os_sandbox = Arc::new(AtomicBool::new(on));
        self
    }

    /// Require the OS sandbox for this run and its already constructed tool views.
    pub fn require_os_sandbox(&self) {
        self.os_sandbox.store(true, Ordering::Relaxed);
    }

    /// Configure the cleared host shell environment and reuse the session’s output redactor.
    pub fn with_host_controls(mut self, layout: Layout, network: Arc<NetSandbox>) -> Self {
        self.host_layout = layout;
        self.host_net = Some(network);
        self
    }

    /// Where a command of `role` runs now.
    pub fn decide(&self, role: ExecRole) -> ExecDecision {
        self.policy
            .decide(role, self.switch.full(), self.os_sandbox.load(Ordering::Relaxed))
    }

    fn role_of(s: &ExecSpec) -> ExecRole {
        if s.writable {
            ExecRole::ModelBash
        } else {
            ExecRole::ReadOnly
        }
    }

    fn host_spec(&self, s: &ExecSpec) -> ExecSpec {
        let mut spec = s.clone();
        let mut env = crate::bwrap::base_env(&self.home.to_string_lossy(), &self.host_layout);
        // The bind layouts keep cache and temp data inside the sandbox. Host execution
        // uses the user's normal cache and temp defaults.
        env.retain(|(key, value)| {
            !(matches!(
                key.as_str(),
                "TMPDIR" | "XDG_CACHE_HOME" | "SCCACHE_DIR" | "SCCACHE_SERVER_UDS"
            ) || key == "MISE_STATE_DIR" && value == "/tmp/mise-state")
        });
        env.extend(
            s.env
                .iter()
                .filter(|(key, _)| {
                    matches!(key.as_str(), "OXA_SESSION_ID" | "OXA_SESSION_FILE")
                        || !crate::is_secret_name(key)
                })
                .cloned(),
        );
        spec.env = env;
        spec
    }

    /// Reuse the process's switch and launch settings with the session's own sandbox.
    /// In particular, auto/plan bash and the rest of the session must share one scratch dir.
    pub fn with_inner(&self, inner: Arc<dyn Sandbox>) -> FullAccess {
        FullAccess {
            inner,
            switch: self.switch.clone(),
            home: self.home.clone(),
            policy: self.policy.clone(),
            os_sandbox: self.os_sandbox.clone(),
            host_layout: self.host_layout.clone(),
            host_net: self.host_net.clone(),
        }
    }

    /// The exec hooks' sandbox: it asks [`ExecRole::Hook`] for every command. Hooks stay in the
    /// wrapped sandbox (refused without bwrap on Linux, never run on the host instead), except
    /// under yolo (startup or live, also a yolo-started unattended worker) and on a platform
    /// without an OS sandbox, where they run with the user's rights like bash, keeping the
    /// offline rule.
    pub fn hook_sandbox(full: &Arc<FullAccess>) -> Arc<dyn Sandbox> {
        Arc::new(HookView { full: full.clone() })
    }

    /// The session's file-tool view follows bash's full-access switch. Process execution
    /// still delegates to `inner`: exec hooks/extensions must not acquire bash's bypass.
    pub fn file_view(&self, inner: Arc<dyn Sandbox>) -> Arc<dyn Sandbox> {
        Arc::new(FileView {
            inner,
            switch: self.switch.clone(),
            os_sandbox: self.os_sandbox.clone(),
        })
    }

    /// The argv `s` is started with: `bwrap` and the wrapped sandbox's arguments, `bash -c
    /// <command>` on the host, or bwrap without network for a host command that may not have it.
    pub fn launch_argv(&self, s: &ExecSpec) -> Result<Vec<String>, OxaError> {
        match self.decide(Self::role_of(s)) {
            ExecDecision::Sandbox => {
                let mut argv = vec!["bwrap".to_string()];
                argv.extend(self.inner.bwrap_args(s));
                Ok(argv)
            }
            ExecDecision::Host => Ok(["bash", "-c", s.command.as_str()].map(str::to_string).to_vec()),
            ExecDecision::HostNoNet(bwrap) => Ok(crate::offline::model_offline_argv(
                &bwrap, &self.home, &s.cwd, &s.command,
            )),
            ExecDecision::Refuse(why) => Err(OxaError::new(oxa_proto::codes::OFFLINE, why)),
        }
    }

    /// Run `s` as `role` says: the wrapped sandbox, or the host with the output sink.
    fn exec(
        &self,
        role: ExecRole,
        s: &ExecSpec,
        stdin: Option<&[u8]>,
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
        capture: bool,
    ) -> ExecResult {
        match self.decide(role) {
            ExecDecision::Sandbox => match stdin {
                Some(input) => self.inner.run_with_stdin(s, input, c, live),
                None if capture => self.inner.run(s, c, live),
                None => self.inner.run_streaming(s, c, live),
            },
            decision => self.run_host(&decision, s, stdin, c, live, capture),
        }
    }

    /// The host side of a decision: with the user's rights (cleared environment), the host
    /// credentials scrubbed from the output at the one sink.
    fn run_host(
        &self,
        decision: &ExecDecision,
        s: &ExecSpec,
        stdin: Option<&[u8]>,
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
        capture: bool,
    ) -> ExecResult {
        let spec = self.host_spec(s);
        match decision {
            ExecDecision::Host => {
                let run = |live: &mut dyn FnMut(&[u8])| match stdin {
                    Some(input) => run_unsandboxed_stdin(&spec, input, c, live),
                    None => run_unsandboxed_capture(&spec, c, live, capture),
                };
                let mut result = self.host_sink(&spec.env, live, run);
                result.unsandboxed = true;
                result
            }
            ExecDecision::HostNoNet(bwrap) => {
                let argv = crate::offline::model_offline_argv(bwrap, &self.home, &s.cwd, &s.command);
                let capture = capture || stdin.is_some();
                self.host_sink(&spec.env, live, |live: &mut dyn FnMut(&[u8])| {
                    crate::run::run_argv_cleared(&argv, &spec.env, stdin, s.timeout_s, c, live, capture)
                })
            }
            ExecDecision::Refuse(why) => {
                let message = why.clone().into_bytes();
                live(&message);
                ExecResult {
                    output: if capture || stdin.is_some() {
                        message
                    } else {
                        Vec::new()
                    },
                    exit: None,
                    timed_out: false,
                    unsandboxed: false,
                }
            }
            ExecDecision::Sandbox => unreachable!("a sandboxed command does not go through the host path"),
        }
    }

    fn host_sink(
        &self,
        env: &[(String, String)],
        live: &mut dyn FnMut(&[u8]),
        run: impl FnOnce(&mut dyn FnMut(&[u8])) -> ExecResult,
    ) -> ExecResult {
        NetSandbox::redact_host_output(self.host_net.as_deref(), env, live, run)
    }
}

impl Sandbox for FullAccess {
    fn check(&self) -> Result<(), OxaError> {
        match self.decide(ExecRole::ModelBash) {
            ExecDecision::Sandbox => self.inner.check(),
            _ => Ok(()),
        }
    }
    fn network(&self) -> bool {
        match self.decide(ExecRole::ModelBash) {
            ExecDecision::Sandbox => self.inner.network(),
            ExecDecision::Host => true,
            ExecDecision::HostNoNet(_) | ExecDecision::Refuse(_) => false,
        }
    }
    fn network_enforced(&self) -> bool {
        self.inner.network_enforced()
    }
    fn persistence_notice(&self) -> Option<&str> {
        match self.decide(ExecRole::ModelBash) {
            ExecDecision::Sandbox => self.inner.persistence_notice(),
            _ => None,
        }
    }
    fn bash_view(&self, cwd: &Path, path: &Path, writable: bool) -> oxa_proto::BashView {
        self.inner.bash_view(cwd, path, writable)
    }
    fn bwrap_args(&self, s: &ExecSpec) -> Vec<String> {
        match self.decide(Self::role_of(s)) {
            ExecDecision::Sandbox => self.inner.bwrap_args(s),
            _ => Vec::new(),
        }
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.exec(Self::role_of(s), s, None, c, live, true)
    }
    fn run_streaming(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.exec(Self::role_of(s), s, None, c, live, false)
    }
    /// Only exec hooks pass standard input; they are the user's own configuration, not the
    /// model's bash, and decide as [`ExecRole::Hook`].
    fn run_with_stdin(
        &self,
        s: &ExecSpec,
        stdin: &[u8],
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        self.exec(ExecRole::Hook, s, Some(stdin), c, live, true)
    }
}

/// The sandbox of exec hooks and extension processes: every command asks for an
/// [`ExecRole::Hook`] decision; everything else is the wrapped sandbox's.
struct HookView {
    full: Arc<FullAccess>,
}

impl Sandbox for HookView {
    fn check(&self) -> Result<(), OxaError> {
        self.full.inner.check()
    }
    fn network(&self) -> bool {
        self.full.inner.network()
    }
    fn network_enforced(&self) -> bool {
        self.full.inner.network_enforced()
    }
    fn persistence_notice(&self) -> Option<&str> {
        self.full.inner.persistence_notice()
    }
    fn home(&self) -> Option<&std::path::Path> {
        self.full.inner.home()
    }
    fn scratch_dir(&self) -> Option<&std::path::Path> {
        self.full.inner.scratch_dir()
    }
    fn preserves_host_path(&self, path: &std::path::Path) -> bool {
        self.full.inner.preserves_host_path(path)
    }
    fn bwrap_args(&self, s: &ExecSpec) -> Vec<String> {
        self.full.inner.bwrap_args(s)
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.full.exec(ExecRole::Hook, s, None, c, live, true)
    }
    fn run_streaming(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.full.exec(ExecRole::Hook, s, None, c, live, false)
    }
    fn run_with_stdin(
        &self,
        s: &ExecSpec,
        stdin: &[u8],
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        self.full.exec(ExecRole::Hook, s, Some(stdin), c, live, true)
    }
}

/// Only path identity varies with full access; all execution stays in the inner sandbox.
struct FileView {
    inner: Arc<dyn Sandbox>,
    switch: YoloSwitch,
    os_sandbox: Arc<AtomicBool>,
}

impl Sandbox for FileView {
    fn check(&self) -> Result<(), OxaError> {
        self.inner.check()
    }
    fn network(&self) -> bool {
        self.inner.network()
    }
    fn network_enforced(&self) -> bool {
        self.inner.network_enforced()
    }
    fn persistence_notice(&self) -> Option<&str> {
        self.inner.persistence_notice()
    }
    fn home(&self) -> Option<&std::path::Path> {
        self.inner.home()
    }
    fn scratch_dir(&self) -> Option<&std::path::Path> {
        self.inner.scratch_dir()
    }
    fn file_scratch_dir(&self, mode: oxa_proto::Mode) -> Option<&std::path::Path> {
        if (self.switch.full() || !self.os_sandbox.load(Ordering::Relaxed)) && mode != oxa_proto::Mode::Plan {
            None
        } else {
            self.inner.file_scratch_dir(mode)
        }
    }
    fn preserves_host_path(&self, path: &std::path::Path) -> bool {
        self.inner.preserves_host_path(path)
    }
    /// Unsandboxed file tools and bash are the same machine.
    fn bash_view(&self, cwd: &Path, path: &Path, writable: bool) -> oxa_proto::BashView {
        if self.switch.full() || !self.os_sandbox.load(Ordering::Relaxed) {
            return oxa_proto::BashView::Same;
        }
        self.inner.bash_view(cwd, path, writable)
    }
    fn for_session(&self) -> Option<Arc<dyn Sandbox>> {
        self.inner.for_session().map(|inner| {
            Arc::new(FileView {
                inner,
                switch: self.switch.clone(),
                os_sandbox: self.os_sandbox.clone(),
            }) as Arc<dyn Sandbox>
        })
    }
    fn bwrap_args(&self, s: &ExecSpec) -> Vec<String> {
        self.inner.bwrap_args(s)
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.inner.run(s, c, live)
    }
    fn run_streaming(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.inner.run_streaming(s, c, live)
    }
    fn run_with_stdin(
        &self,
        s: &ExecSpec,
        stdin: &[u8],
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        self.inner.run_with_stdin(s, stdin, c, live)
    }
}
