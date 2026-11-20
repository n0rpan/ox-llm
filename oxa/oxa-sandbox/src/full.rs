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
use crate::{run_unsandboxed_capture, run_unsandboxed_stdin, Layout, NetSandbox};
use oxa_proto::{CancelToken, ExecResult, ExecSpec, OxaError, Sandbox, YoloSwitch};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;

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
    os_sandbox: Arc<AtomicBool>,
    host_layout: Layout,
    host_net: Option<Arc<NetSandbox>>,
    /// The bwrap of the network-disabled host path, found and probed once. It is resolved when
    /// the run is built offline or with `oxa.sandbox.net = off`, before any model command: a
    /// later `PATH` lookup could pick up a `bwrap` a command placed in a writable `PATH` entry
    /// and run it (the probe included) with the network. Host bash can write what the user
    /// can, so the pinned file and every directory above it must be root-owned and not group-
    /// or world-writable ([`root_owned_path`]); otherwise it could be swapped after the pin.
    /// `None` when no such bwrap was usable.
    offline_bwrap: Arc<OnceLock<Option<PathBuf>>>,
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
            os_sandbox: Arc::new(AtomicBool::new(true)),
            host_layout: Layout::default(),
            host_net: None,
            offline_bwrap: Arc::new(OnceLock::new()),
        }
        .pin_offline_bwrap()
    }

    /// Resolve the network-disabled bwrap now when this run may need it (see `offline_bwrap`).
    fn pin_offline_bwrap(self) -> FullAccess {
        if self.offline || self.net_off {
            self.offline_bwrap();
        }
        self
    }

    fn offline_bwrap(&self) -> Option<&Path> {
        self.offline_bwrap
            .get_or_init(|| {
                let found = crate::Bwrap::detect(&self.path_var, &self.home).ok()?.bwrap;
                let real = std::fs::canonicalize(found).ok()?;
                root_owned_path(&real).then_some(real)
            })
            .as_deref()
    }

    fn offline_argv(&self, s: &ExecSpec) -> Result<Vec<String>, OxaError> {
        let bwrap = self
            .offline_bwrap()
            .ok_or_else(|| oxa_proto::offline_error("bash under full access"))?;
        Ok(crate::offline::model_offline_argv(
            bwrap, &self.home, &s.cwd, &s.command,
        ))
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
            path_var: self.path_var.clone(),
            home: self.home.clone(),
            offline: self.offline,
            net_off: self.net_off,
            os_sandbox: self.os_sandbox.clone(),
            host_layout: self.host_layout.clone(),
            host_net: self.host_net.clone(),
            offline_bwrap: self.offline_bwrap.clone(),
        }
    }

    /// The exec hooks' sandbox. Hooks are trusted configuration and keep the wrapped sandbox,
    /// except under yolo: the owner standard is "yolo = no sandbox anywhere", so while the
    /// switch is at yolo (startup or live, also a yolo-started unattended worker) they run with
    /// the user's rights like bash, keeping the offline rule.
    pub fn hook_sandbox(full: &Arc<FullAccess>) -> Arc<dyn Sandbox> {
        full.inner.clone()
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

    /// Commands run with the user's rights: full access is on, or there is no OS sandbox.
    fn unsandboxed(&self) -> bool {
        self.switch.full() || !self.os_sandbox.load(Ordering::Relaxed)
    }

    /// An unsandboxed command gets no network when the process is offline or the owner set
    /// `oxa.sandbox.net` to `off`; it then runs through bwrap without network (or is refused).
    fn no_net(&self) -> bool {
        self.offline || (self.net_off && self.unsandboxed())
    }

    /// `s` bypasses the sandbox when the run is unsandboxed and the spec may write. Read-only
    /// plan specs always delegate to the wrapped sandbox and fail closed if it is unavailable.
    fn bypass(&self, s: &ExecSpec) -> bool {
        if self.unsandboxed() && s.writable {
            return true;
        }
        false
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
            return self.offline_argv(s);
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
            let spec = self.host_spec(s);
            let mut result = if let Some(net) = &self.host_net {
                net.redact_host_output(live, |live| run_unsandboxed_capture(&spec, c, live, capture))
            } else {
                run_unsandboxed_capture(&spec, c, live, capture)
            };
            result.unsandboxed = true;
            return result;
        }
        self.run_without_network(s, None, c, live, capture)
    }

    fn run_without_network(
        &self,
        s: &ExecSpec,
        stdin: Option<&[u8]>,
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
        capture: bool,
    ) -> ExecResult {
        match self.offline_argv(s) {
            Ok(argv) => {
                let spec = self.host_spec(s);
                let run = |live: &mut dyn FnMut(&[u8])| {
                    crate::run::run_argv_cleared(&argv, &spec.env, stdin, s.timeout_s, c, live, capture)
                };
                // The network-disabled path still reads the same token files: scrub them too.
                match &self.host_net {
                    Some(net) => net.redact_host_output(live, run),
                    None => run(live),
                }
            }
            Err(_) => {
                let message = oxa_proto::offline_error("bash under full access")
                    .to_string()
                    .into_bytes();
                live(&message);
                ExecResult {
                    output: if capture { message } else { Vec::new() },
                    exit: None,
                    timed_out: false,
                    unsandboxed: false,
                }
            }
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
    fn network_enforced(&self) -> bool {
        self.inner.network_enforced()
    }
    fn persistence_notice(&self) -> Option<&str> {
        if self.unsandboxed() {
            return None;
        }
        self.inner.persistence_notice()
    }
    fn bash_view(&self, cwd: &Path, path: &Path, writable: bool) -> oxa_proto::BashView {
        self.inner.bash_view(cwd, path, writable)
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
    /// bash, keep the owner's network setting, and start from an empty environment plus the
    /// hook's own (never the yolo inherit path).
    fn run_with_stdin(
        &self,
        s: &ExecSpec,
        stdin: &[u8],
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        if self.os_sandbox.load(Ordering::Relaxed) {
            return self.inner.run_with_stdin(s, stdin, c, live);
        }
        if !self.no_net() {
            let spec = self.host_spec(s);
            if let Some(net) = &self.host_net {
                return net.redact_host_output(live, |live| run_unsandboxed_stdin(&spec, stdin, c, live));
            }
            return run_unsandboxed_stdin(&spec, stdin, c, live);
        }
        self.run_without_network(s, Some(stdin), c, live, true)
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

/// `true` when `path` (canonical) and every directory above it are owned by root and not
/// writable by group or others, so a command running with the user's rights cannot replace it.
fn root_owned_path(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    path.ancestors()
        .all(|p| std::fs::metadata(p).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o022 == 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoSandbox;
    use oxa_fake::TestEnv;
    use oxa_proto::Autonomy;

    /// The network-disabled host path runs the pinned bwrap with the cleared environment: no
    /// secret variables, the session variables kept. A fake bwrap stands in for the pinned one
    /// (a user-owned file is never pinned by `offline_bwrap` itself).
    #[test]
    fn network_disabled_host_bash_uses_the_cleared_environment() {
        use std::os::unix::fs::PermissionsExt;
        let env = TestEnv::new();
        let fake = env.write(
            "bin/bwrap",
            "#!/bin/sh\n[ \"$1\" = --unshare-net ] || exit 1\n[ -z \"${GH_TOKEN+x}\" ] || exit 2\nprintf '%s|%s' \"$HOME\" \"$OXA_SESSION_ID\"\n",
        );
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut sb = FullAccess::new(
            Arc::new(NoSandbox),
            YoloSwitch::new(Autonomy::Standard),
            "",
            env.home(),
            false,
            false,
        )
        .with_os_sandbox(false);
        sb.net_off = true;
        assert!(sb.offline_bwrap.set(Some(fake)).is_ok());
        let spec = ExecSpec {
            command: "true".into(),
            cwd: env.work(),
            writable: true,
            net: false,
            timeout_s: Some(30),
            env: vec![
                ("GH_TOKEN".into(), "fixture-value".into()),
                ("OXA_SESSION_ID".into(), "fixture-session".into()),
            ],
        };
        let result = sb.run(&spec, &CancelToken::new(), &mut |_| {});
        assert_eq!(
            result.exit,
            Some(0),
            "{}",
            String::from_utf8_lossy(&result.output)
        );
        assert_eq!(
            result.output,
            format!("{}|fixture-session", env.home().display()).as_bytes()
        );
    }
}
