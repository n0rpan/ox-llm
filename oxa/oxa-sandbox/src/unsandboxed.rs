//! Platforms without an OS sandbox (anything but Linux, where bubblewrap does not exist).
//! Commands run as `bash -c` with the user's own rights; there is no isolation and no pretence
//! of any. Every result is flagged `unsandboxed` so a front end can warn, and the notice
//! below goes to the model. Protections that live inside oxa (the file tools' denials, credential
//! masking, the network setting in [`crate::NetSandbox`]) are unaffected.
use crate::run::{run_unsandboxed_capture, run_unsandboxed_stdin};
use crate::NO_OS_SANDBOX_NOTICE;
use oxa_proto::{CancelToken, ExecResult, ExecSpec, OxaError, Sandbox};

/// The bash tool's executor where no OS sandbox exists.
#[derive(Debug, Clone, Copy, Default)]
pub struct Unsandboxed;

impl Sandbox for Unsandboxed {
    /// Always usable: there is nothing to probe. It does not claim isolation.
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn network(&self) -> bool {
        true
    }
    fn persistence_notice(&self) -> Option<&str> {
        Some(NO_OS_SANDBOX_NOTICE)
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        run_unsandboxed_capture(s, c, live, true)
    }
    fn run_streaming(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        run_unsandboxed_capture(s, c, live, false)
    }
    fn run_with_stdin(
        &self,
        s: &ExecSpec,
        stdin: &[u8],
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        run_unsandboxed_stdin(s, stdin, c, live)
    }
}
