//! O-167(b) (wave H lane `local`): the bash network as a sandbox property.
//!
//! The agent's tools get the internet by default; the harness itself never phones home.
//! [`NetSandbox`] wraps the process sandbox and answers [`Sandbox::network`] from the setting
//! `oxa.sandbox.net` (`on`, the default, or `off`) and the offline switch (`OXA_OFFLINE=1`,
//! `--offline`, `oxa.offline`: the network is off whatever the setting). It also clamps every
//! spec it runs, so with the network off no argv carries `--share-net`, whoever built the spec
//! (an extension's `exec` with `net: true` included).
use oxa_proto::{CancelToken, ExecResult, ExecSpec, OxaError, Sandbox};
use std::sync::Arc;

/// The setting key.
pub const SETTING_SANDBOX_NET: &str = "oxa.sandbox.net";

/// `on` (or absent, or any other value: the schema reports it) = true; `off` = false.
pub fn sandbox_net_setting(value: Option<&str>) -> bool {
    value != Some("off")
}

/// A sandbox with the network decision of the process.
pub struct NetSandbox {
    inner: Arc<dyn Sandbox>,
    net: bool,
}

impl NetSandbox {
    /// `setting_on` = [`sandbox_net_setting`]; `offline` = the hard switch.
    pub fn new(inner: Arc<dyn Sandbox>, setting_on: bool, offline: bool) -> NetSandbox {
        NetSandbox {
            inner,
            net: setting_on && !offline,
        }
    }

    fn clamp(&self, s: &ExecSpec) -> ExecSpec {
        let mut s = s.clone();
        s.net = s.net && self.net;
        s
    }
}

impl Sandbox for NetSandbox {
    fn check(&self) -> Result<(), OxaError> {
        self.inner.check()
    }
    fn network(&self) -> bool {
        self.net
    }
    fn bwrap_args(&self, s: &ExecSpec) -> Vec<String> {
        self.inner.bwrap_args(&self.clamp(s))
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.inner.run(&self.clamp(s), c, live)
    }
    fn run_with_stdin(
        &self,
        s: &ExecSpec,
        stdin: &[u8],
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        self.inner.run_with_stdin(&self.clamp(s), stdin, c, live)
    }
}
