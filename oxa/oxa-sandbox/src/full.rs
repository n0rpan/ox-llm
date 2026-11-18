//! #202: full access (`yolo`). The bash tool's commands run without bwrap, as `bash -c` with
//! the user's own rights and environment, while the process's [`YoloSwitch`] is at `yolo`.
//! A read-only spec (plan mode) and every other level go to the wrapped sandbox unchanged.
//! Offline stays a hard switch: the command then runs through bwrap with the network unshared
//! and the user's file system (as `!cmd` does offline), and is refused without bwrap.
//!
//! #1512 S3: without the OS sandbox (`os_sandbox: false`, the interactive default on Linux)
//! bash and the exec hooks run with the user's rights too; the approval floor of the policy
//! asks about dangerous commands. The owner's `oxa.sandbox.net = off` then holds: such a
//! command goes through bwrap without network, and is refused without bwrap.
use crate::{offline_argv, run_argv_capture, run_argv_stdin, run_unsandboxed_capture, run_unsandboxed_stdin};
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
    /// `oxa.sandbox.net` is `off`: unsandboxed commands get no network either.
    net_off: bool,
    /// #1512 S3: the OS sandbox is part of this run's policy (default on).
    os_sandbox: bool,
}

impl FullAccess {
    /// `path_var` and `home` locate bwrap for an offline run; `offline` is the hard switch;
    /// `net_off` is the owner's `oxa.sandbox.net = off`.
    pub fn new(
        inner: Arc<dyn Sandbox>,
        switch: YoloSwitch,
        path_var: &str,
        home: PathBuf,
        offline: bool,
        net_off: bool,
    ) -> FullAccess {
        FullAccess {
            inner,
            switch,
            path_var: path_var.to_string(),
            home,
            offline,
            net_off,
            os_sandbox: true,
        }
    }

    /// #1512 S3: `false` runs bash and the exec hooks with the user's rights (no OS sandbox).
    pub fn with_os_sandbox(mut self, on: bool) -> FullAccess {
        self.os_sandbox = on;
        self
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
            net_off: self.net_off,
            os_sandbox: self.os_sandbox,
        }
    }

    /// The session's file-tool view follows bash's full-access switch. Process execution
    /// still delegates to `inner`: exec hooks/extensions must not acquire bash's bypass.
    pub fn file_view(&self, inner: Arc<dyn Sandbox>) -> Arc<dyn Sandbox> {
        Arc::new(FileView {
            inner,
            switch: self.switch.clone(),
            os_sandbox: self.os_sandbox,
        })
    }

    /// Commands run with the user's rights: full access is on, or there is no OS sandbox.
    fn unsandboxed(&self) -> bool {
        self.switch.full() || !self.os_sandbox
    }

    /// An unsandboxed command gets no network when the process is offline or the owner set
    /// `oxa.sandbox.net` to `off`; it then runs through bwrap without network (or is refused).
    fn no_net(&self) -> bool {
        self.offline || (self.net_off && self.unsandboxed())
    }

    /// `s` bypasses the sandbox: the run is unsandboxed and the spec may write (not plan mode).
    /// Without the OS sandbox a read-only spec (plan) also runs unsandboxed when bwrap is
    /// missing; its policy already allows only read-only commands.
    fn bypass(&self, s: &ExecSpec) -> bool {
        if self.unsandboxed() && s.writable {
            return true;
        }
        !self.os_sandbox && !s.writable && self.inner.check().is_err()
    }

    /// The argv `s` is started with: `bash -c <command>` when unsandboxed (bwrap without
    /// network when no network is allowed), else `bwrap` and the wrapped sandbox's arguments.
    pub fn launch_argv(&self, s: &ExecSpec) -> Result<Vec<String>, OxaError> {
        if !self.bypass(s) {
            let mut argv = vec!["bwrap".to_string()];
            argv.extend(self.inner.bwrap_args(s));
            return Ok(argv);
        }
        let argv = ["bash", "-c", s.command.as_str()].map(str::to_string).to_vec();
        if self.no_net() {
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
        if !self.no_net() {
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
            },
        }
    }
}

impl Sandbox for FullAccess {
    fn check(&self) -> Result<(), OxaError> {
        if self.unsandboxed() {
            return Ok(());
        }
        self.inner.check()
    }
    fn network(&self) -> bool {
        if self.unsandboxed() {
            return !self.no_net();
        }
        self.inner.network()
    }
    fn persistence_notice(&self) -> Option<&str> {
        if self.unsandboxed() {
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
    /// Only exec hooks pass standard input; they are the user's own configuration, not the
    /// model's bash. With the OS sandbox on they stay in the wrapped sandbox at every level
    /// (no full-access bypass). Without it (#1512 S3) they run with the user's rights, like
    /// bash, and keep the owner's network setting.
    fn run_with_stdin(
        &self,
        s: &ExecSpec,
        stdin: &[u8],
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        if self.os_sandbox {
            return self.inner.run_with_stdin(s, stdin, c, live);
        }
        if !self.no_net() {
            return run_unsandboxed_stdin(s, stdin, c, live);
        }
        let argv = ["bash", "-c", s.command.as_str()].map(str::to_string).to_vec();
        match offline_argv(&self.path_var, &self.home, &s.cwd, &s.env, &argv) {
            Ok(argv) => run_argv_stdin(&argv, &[], stdin, s.timeout_s, c, live),
            Err(_) => ExecResult {
                output: oxa_proto::offline_error("exec hooks").to_string().into_bytes(),
                exit: None,
                timed_out: false,
            },
        }
    }
}

/// Only path identity varies with full access; all execution stays in the inner sandbox.
struct FileView {
    inner: Arc<dyn Sandbox>,
    switch: YoloSwitch,
    os_sandbox: bool,
}

impl Sandbox for FileView {
    fn check(&self) -> Result<(), OxaError> {
        self.inner.check()
    }
    fn network(&self) -> bool {
        self.inner.network()
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
        if (self.switch.full() || !self.os_sandbox) && mode != oxa_proto::Mode::Plan {
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
                os_sandbox: self.os_sandbox,
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
