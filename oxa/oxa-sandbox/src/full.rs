//! #202: full access (`yolo`). The bash tool's commands run without bwrap, as `bash -c` with
//! the user's own rights and environment, while the process's [`YoloSwitch`] is at `yolo`.
//! A read-only spec (plan mode) and every other level go to the wrapped sandbox unchanged.
//! Offline stays a hard switch: the command then runs through bwrap with the network unshared
//! and the user's file system (as `!cmd` does offline), and is refused without bwrap.
use crate::run::run_unsandboxed_stdin_inherit;
use crate::{offline_argv, run_argv_capture, run_argv_stdin, run_unsandboxed_capture};
use oxa_proto::{CancelToken, ExecResult, ExecSpec, OxaError, Sandbox, YoloSwitch};
use std::path::PathBuf;
use std::sync::Arc;

/// The bash tool's sandbox with the full-access bypass of `yolo`.
pub struct FullAccess {
    inner: Arc<dyn Sandbox>,
    switch: YoloSwitch,
    path_var: String,
    home: PathBuf,
    offline: bool,
}

impl FullAccess {
    /// `path_var` and `home` locate bwrap for an offline run; `offline` is the hard switch.
    pub fn new(
        inner: Arc<dyn Sandbox>,
        switch: YoloSwitch,
        path_var: &str,
        home: PathBuf,
        offline: bool,
    ) -> FullAccess {
        FullAccess {
            inner,
            switch,
            path_var: path_var.to_string(),
            home,
            offline,
        }
    }

    /// Reuse the process's switch and launch settings with the session's own sandbox.
    /// In particular, auto/plan bash and the rest of the session must share one scratch dir.
    pub fn with_inner(&self, inner: Arc<dyn Sandbox>) -> FullAccess {
        FullAccess {
            inner,
            switch: self.switch.clone(),
            path_var: self.path_var.clone(),
            home: self.home.clone(),
            offline: self.offline,
        }
    }

    /// The exec hooks' sandbox. Hooks are trusted configuration and keep the wrapped sandbox,
    /// except under yolo: the owner standard is "yolo = no sandbox anywhere", so while the
    /// switch is at yolo (startup or live, also a yolo-started unattended worker) they run with
    /// the user's rights like bash, keeping the offline rule.
    pub fn hook_sandbox(full: &Arc<FullAccess>) -> Arc<dyn Sandbox> {
        Arc::new(HookView { full: full.clone() })
    }

    /// The session's file-tool view follows bash's full-access switch. Process execution
    /// still delegates to `inner`: exec hooks/extensions must not acquire bash's bypass.
    pub fn file_view(&self, inner: Arc<dyn Sandbox>) -> Arc<dyn Sandbox> {
        Arc::new(FileView {
            inner,
            switch: self.switch.clone(),
        })
    }

    /// `s` bypasses the sandbox: full access is on and the spec may write (not plan mode).
    fn bypass(&self, s: &ExecSpec) -> bool {
        self.switch.full() && s.writable
    }

    /// The argv `s` is started with: `bash -c <command>` under full access (bwrap without
    /// network when offline), else `bwrap` and the wrapped sandbox's arguments.
    pub fn launch_argv(&self, s: &ExecSpec) -> Result<Vec<String>, OxaError> {
        if !self.bypass(s) {
            let mut argv = vec!["bwrap".to_string()];
            argv.extend(self.inner.bwrap_args(s));
            return Ok(argv);
        }
        let argv = ["bash", "-c", s.command.as_str()].map(str::to_string).to_vec();
        if self.offline {
            return offline_argv(&self.path_var, &self.home, &s.cwd, &s.env, &argv)
                .map_err(|_| oxa_proto::offline_error("bash under full access"));
        }
        Ok(argv)
    }

    fn run_full(
        &self,
        s: &ExecSpec,
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
        capture: bool,
    ) -> ExecResult {
        if !self.offline {
            return run_unsandboxed_capture(s, c, live, capture);
        }
        match self.launch_argv(s) {
            Ok(argv) => run_argv_capture(&argv, &[], s.timeout_s, c, live, capture),
            Err(e) => ExecResult {
                output: if capture {
                    e.to_string().into_bytes()
                } else {
                    Vec::new()
                },
                exit: None,
                timed_out: false,
                unsandboxed: false,
            },
        }
    }
}

impl Sandbox for FullAccess {
    fn check(&self) -> Result<(), OxaError> {
        if self.switch.full() {
            return Ok(());
        }
        self.inner.check()
    }
    fn network(&self) -> bool {
        if self.switch.full() {
            return !self.offline;
        }
        self.inner.network()
    }
    fn network_enforced(&self) -> bool {
        self.inner.network_enforced()
    }
    fn persistence_notice(&self) -> Option<&str> {
        if self.switch.full() {
            return None;
        }
        self.inner.persistence_notice()
    }
    fn bwrap_args(&self, s: &ExecSpec) -> Vec<String> {
        if self.bypass(s) {
            return Vec::new();
        }
        self.inner.bwrap_args(s)
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        if self.bypass(s) {
            return self.run_full(s, c, live, true);
        }
        self.inner.run(s, c, live)
    }
    fn run_streaming(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        if self.bypass(s) {
            return self.run_full(s, c, live, false);
        }
        self.inner.run_streaming(s, c, live)
    }
    /// Only exec hooks pass standard input, through `hook_sandbox`. Called directly, this
    /// stays in the wrapped sandbox at every level.
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

impl FullAccess {
    /// `run_with_stdin` with the user's rights (bash under yolo), keeping the offline rule.
    fn run_host_stdin(
        &self,
        s: &ExecSpec,
        stdin: &[u8],
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        if !self.offline {
            return run_unsandboxed_stdin_inherit(s, stdin, c, live);
        }
        match self.launch_argv(s) {
            Ok(argv) => run_argv_stdin(&argv, &[], stdin, s.timeout_s, c, live),
            Err(e) => ExecResult {
                output: e.to_string().into_bytes(),
                exit: None,
                timed_out: false,
                unsandboxed: false,
            },
        }
    }
}

/// The exec hooks' sandbox (`FullAccess::hook_sandbox`): the wrapped sandbox, except while the
/// switch is at yolo, when writable hooks run unsandboxed like bash.
struct HookView {
    full: Arc<FullAccess>,
}

impl HookView {
    fn yolo(&self, s: &ExecSpec) -> bool {
        self.full.bypass(s)
    }
}

impl Sandbox for HookView {
    fn check(&self) -> Result<(), OxaError> {
        if self.full.switch.full() {
            return Ok(());
        }
        self.full.inner.check()
    }
    fn network(&self) -> bool {
        if self.full.switch.full() {
            return !self.full.offline;
        }
        self.full.inner.network()
    }
    fn network_enforced(&self) -> bool {
        self.full.inner.network_enforced()
    }
    fn persistence_notice(&self) -> Option<&str> {
        if self.full.switch.full() {
            return None;
        }
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
        if self.yolo(s) {
            return Vec::new();
        }
        self.full.inner.bwrap_args(s)
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        if self.yolo(s) {
            return self.full.run_full(s, c, live, true);
        }
        self.full.inner.run(s, c, live)
    }
    fn run_streaming(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        if self.yolo(s) {
            return self.full.run_full(s, c, live, false);
        }
        self.full.inner.run_streaming(s, c, live)
    }
    fn run_with_stdin(
        &self,
        s: &ExecSpec,
        stdin: &[u8],
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        if self.yolo(s) {
            return self.full.run_host_stdin(s, stdin, c, live);
        }
        self.full.inner.run_with_stdin(s, stdin, c, live)
    }
}

/// Only path identity varies with full access; all execution stays in the inner sandbox.
struct FileView {
    inner: Arc<dyn Sandbox>,
    switch: YoloSwitch,
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
        if self.switch.full() && mode != oxa_proto::Mode::Plan {
            None
        } else {
            self.inner.file_scratch_dir(mode)
        }
    }
    fn preserves_host_path(&self, path: &std::path::Path) -> bool {
        self.inner.preserves_host_path(path)
    }
    fn for_session(&self) -> Option<Arc<dyn Sandbox>> {
        self.inner.for_session().map(|inner| {
            Arc::new(FileView {
                inner,
                switch: self.switch.clone(),
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
