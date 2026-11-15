//! #202: full access (`yolo`). The bash tool's commands run without bwrap, as `bash -c` with
//! the user's own rights and environment, while the process's [`YoloSwitch`] is at `yolo`.
//! A read-only spec (plan mode) and every other level go to the wrapped sandbox unchanged.
//! Offline stays a hard switch: the command then runs through bwrap with the network unshared
//! and the user's file system (as `!cmd` does offline), and is refused without bwrap.
use crate::{offline_argv, run_argv, run_argv_capture, run_unsandboxed_capture};
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
            Ok(argv) if capture => run_argv(&argv, &[], s.timeout_s, c, live),
            Ok(argv) => run_argv_capture(&argv, &[], s.timeout_s, c, live, false),
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
