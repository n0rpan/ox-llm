//! Platforms without an OS sandbox (anything but Linux, where bubblewrap does not exist).
//! Commands run as `bash -c` with the user's own rights; there is no isolation and no pretence
//! of any. Every result is flagged `unsandboxed` so a front end can warn, and the notice
//! below goes to the model. Protections that live inside oxa (the file tools' denials, credential
//! masking, the network setting in [`crate::NetSandbox`]) are unaffected.
use crate::bwrap::base_env;
use crate::layout::Layout;
use crate::run::{run_unsandboxed_env, run_unsandboxed_stdin};
use crate::NO_OS_SANDBOX_NOTICE;
use oxa_proto::{CancelToken, ExecResult, ExecSpec, OxaError, Sandbox};
use std::path::PathBuf;

/// The bash tool's executor where no OS sandbox exists. It cannot enforce the network setting
/// ([`Sandbox::network_enforced`] is false), but it gives commands the same cleared base
/// environment as bwrap: credential variables of the process are not inherited.
#[derive(Debug, Clone, Default)]
pub struct Unsandboxed {
    /// `PATH` and the passed-through variables, as the bwrap layout has them.
    pub layout: Layout,
    /// `$HOME` for the command (the real one: there is no sandbox home).
    pub home: PathBuf,
}

impl Unsandboxed {
    fn env(&self, s: &ExecSpec) -> Vec<(String, String)> {
        base_env(s, &self.home.to_string_lossy(), &self.layout)
    }
}

impl Sandbox for Unsandboxed {
    /// Always usable: there is nothing to probe. It does not claim isolation.
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn network(&self) -> bool {
        true
    }
    fn network_enforced(&self) -> bool {
        false
    }
    fn persistence_notice(&self) -> Option<&str> {
        Some(NO_OS_SANDBOX_NOTICE)
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        let mut result = run_unsandboxed_env(s, Some(&self.env(s)), c, live, true);
        result.unsandboxed = true;
        result
    }
    fn run_streaming(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        let mut result = run_unsandboxed_env(s, Some(&self.env(s)), c, live, false);
        result.unsandboxed = true;
        result
    }
    fn run_with_stdin(
        &self,
        s: &ExecSpec,
        stdin: &[u8],
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        run_unsandboxed_stdin(s, stdin, c, live, &self.env(s))
    }
}
