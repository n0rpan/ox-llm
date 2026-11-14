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
    gh_token: Option<String>,
}

/// The replacement for a redacted token in tool output.
const REDACTED: &str = "[redacted]";

/// Exact-match replace of `token` in `bytes`.
fn redact(bytes: &[u8], token: &str) -> Vec<u8> {
    let t = token.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(t) {
            out.extend_from_slice(REDACTED.as_bytes());
            i += t.len();
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// A streaming redactor: holds back at most `token.len() - 1` bytes (a suffix that could be the
/// start of the token) between chunks; [`StreamRedactor::finish`] flushes them at EOF.
struct StreamRedactor<'a> {
    token: &'a [u8],
    pending: Vec<u8>,
}

impl<'a> StreamRedactor<'a> {
    fn new(token: &'a str) -> Self {
        StreamRedactor { token: token.as_bytes(), pending: Vec::new() }
    }

    /// Feed a chunk; returns the bytes now safe to show.
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let t = self.token;
        let mut out = Vec::new();
        let mut i = 0;
        let buf = &self.pending;
        while i < buf.len() {
            let rest = &buf[i..];
            if rest.starts_with(t) {
                out.extend_from_slice(REDACTED.as_bytes());
                i += t.len();
            } else if t.starts_with(rest) {
                break; // maybe the start of the token: wait for more
            } else {
                out.push(buf[i]);
                i += 1;
            }
        }
        self.pending.drain(..i);
        out
    }

    /// EOF: whatever is held back is not a token.
    fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

impl NetSandbox {
    /// `setting_on` = [`sandbox_net_setting`]; `offline` = the hard switch.
    pub fn new(inner: Arc<dyn Sandbox>, setting_on: bool, offline: bool) -> NetSandbox {
        NetSandbox {
            inner,
            net: setting_on && !offline,
            gh_token: None,
        }
    }

    /// #52: the GitHub token (from `gh auth token`, read outside the sandbox) to pass to
    /// commands that have the network, as `GH_TOKEN` with `GH_HOST=github.com`. Ignored when the
    /// network is off (offline, `oxa.sandbox.net: off`), empty or containing whitespace. The
    /// token is only in the child's environment; it is replaced in the output of the command
    /// and never shown in [`Sandbox::bwrap_args`].
    pub fn with_gh_token(mut self, token: Option<String>) -> NetSandbox {
        self.gh_token = token
            .map(|t| t.trim().to_string())
            .filter(|t| self.net && !t.is_empty() && !t.chars().any(char::is_whitespace));
        self
    }

    /// The spec for a real run: the clamp plus the token, when the command has the network.
    fn clamp_run(&self, s: &ExecSpec) -> ExecSpec {
        let mut s = self.clamp(s);
        if let (true, Some(t)) = (s.net, &self.gh_token) {
            s.env.retain(|(k, _)| k != "GH_TOKEN" && k != "GH_HOST");
            s.env.push(("GH_TOKEN".into(), t.clone()));
            s.env.push(("GH_HOST".into(), "github.com".into()));
        }
        s
    }

    fn redacted_run(
        &self,
        spec: &ExecSpec,
        live: &mut dyn FnMut(&[u8]),
        run: impl FnOnce(&ExecSpec, &mut dyn FnMut(&[u8])) -> ExecResult,
    ) -> ExecResult {
        let s = self.clamp_run(spec);
        let Some(t) = self.gh_token.as_ref().filter(|_| s.net) else {
            return run(&s, live);
        };
        let mut sr = StreamRedactor::new(t);
        let mut r = run(&s, &mut |b: &[u8]| {
            let o = sr.push(b);
            if !o.is_empty() {
                live(&o);
            }
        });
        let tail = sr.finish();
        if !tail.is_empty() {
            live(&tail);
        }
        r.output = redact(&r.output, t);
        r
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
        self.redacted_run(s, live, |s, live| self.inner.run(s, c, live))
    }
    fn run_with_stdin(
        &self,
        s: &ExecSpec,
        stdin: &[u8],
        c: &CancelToken,
        live: &mut dyn FnMut(&[u8]),
    ) -> ExecResult {
        self.redacted_run(s, live, |s, live| self.inner.run_with_stdin(s, stdin, c, live))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records the env of each run and answers with the token echoed back.
    struct Echo(Mutex<Vec<Vec<(String, String)>>>);
    impl Sandbox for Echo {
        fn check(&self) -> Result<(), OxaError> {
            Ok(())
        }
        fn bwrap_args(&self, s: &ExecSpec) -> Vec<String> {
            s.env.iter().map(|(k, v)| format!("{k}={v}")).collect()
        }
        fn run(&self, s: &ExecSpec, _c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
            self.0.lock().unwrap().push(s.env.clone());
            let out = format!(
                "token is {}\n",
                s.env
                    .iter()
                    .find(|(k, _)| k == "GH_TOKEN")
                    .map_or("none", |(_, v)| v)
            );
            live(out.as_bytes());
            ExecResult {
                output: out.into_bytes(),
                exit: Some(0),
                timed_out: false,
            }
        }
    }

    fn spec(net: bool) -> ExecSpec {
        ExecSpec {
            command: "x".into(),
            cwd: "/".into(),
            writable: true,
            net,
            timeout_s: None,
            env: vec![("OXA_SESSION_ID".into(), "s".into())],
        }
    }

    fn run(setting: bool, offline: bool, net: bool) -> (Vec<(String, String)>, String, String) {
        let inner = Arc::new(Echo(Mutex::new(Vec::new())));
        let sb =
            NetSandbox::new(inner.clone(), setting, offline).with_gh_token(Some("ghp_SECRET123\n".into()));
        let mut live = Vec::new();
        let r = sb.run(&spec(net), &CancelToken::new(), &mut |b| {
            live.extend_from_slice(b)
        });
        let env = inner.0.lock().unwrap().pop().unwrap();
        (
            env,
            String::from_utf8(r.output).unwrap(),
            String::from_utf8(live).unwrap(),
        )
    }

    #[test]
    fn token_is_in_the_env_online_and_redacted_in_output() {
        let (env, out, live) = run(true, false, true);
        assert!(
            env.contains(&("GH_TOKEN".into(), "ghp_SECRET123".into())),
            "{env:?}"
        );
        assert!(env.contains(&("GH_HOST".into(), "github.com".into())));
        assert_eq!(out, "token is [redacted]\n");
        assert_eq!(live, out);
    }

    #[test]
    fn token_is_absent_offline_and_without_network() {
        for (setting, offline, net) in [(true, true, true), (false, false, true), (true, false, false)] {
            let (env, out, _) = run(setting, offline, net);
            assert!(
                !env.iter().any(|(k, _)| k == "GH_TOKEN" || k == "GH_HOST"),
                "{env:?}"
            );
            assert_eq!(out, "token is none\n");
        }
    }

    #[test]
    fn token_never_shows_in_the_argv_display() {
        let sb = NetSandbox::new(Arc::new(Echo(Mutex::new(Vec::new()))), true, false)
            .with_gh_token(Some("ghp_SECRET123".into()));
        let shown = sb.bwrap_args(&spec(true)).join(" ");
        assert!(!shown.contains("ghp_SECRET123"), "{shown}");
    }

    /// Emits "a <token> b" in two chunks split at `self.0`.
    struct Split(usize);
    impl Sandbox for Split {
        fn check(&self) -> Result<(), OxaError> {
            Ok(())
        }
        fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, _s: &ExecSpec, _c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
            let out = b"a SYNTH_MARKER_TOKEN_42 b SYNTH_MARKER_TOKEN_42".to_vec();
            live(&out[..self.0]);
            live(&out[self.0..]);
            ExecResult { output: out, exit: Some(0), timed_out: false }
        }
    }

    #[test]
    fn a_token_split_across_live_chunks_never_leaks() {
        let total = "a SYNTH_MARKER_TOKEN_42 b SYNTH_MARKER_TOKEN_42".len();
        for at in 0..=total {
            let sb = NetSandbox::new(Arc::new(Split(at)), true, false)
                .with_gh_token(Some("SYNTH_MARKER_TOKEN_42".into()));
            let mut live = Vec::new();
            let r = sb.run(&spec(true), &CancelToken::new(), &mut |b| live.extend_from_slice(b));
            let live = String::from_utf8(live).unwrap();
            assert_eq!(live, "a [redacted] b [redacted]", "split at {at}");
            assert_eq!(String::from_utf8(r.output).unwrap(), live, "split at {at}");
        }
    }
}
