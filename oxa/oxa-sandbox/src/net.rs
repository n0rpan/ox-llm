//! O-167(b) (wave H): the bash network as a sandbox property.
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
    /// Shared by the per-session copies ([`Sandbox::for_session`]): the token is looked up
    /// once per process, and redaction knows it in every later session.
    gh_token: Arc<std::sync::OnceLock<Option<String>>>,
    gh_source: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    /// Every credential the owner's GitHub CLI holds (all accounts and hosts), found lazily
    /// once for output redaction on host-rights paths. Unlike `gh_token` this does not depend
    /// on the bash network: a token file is readable with the network off too.
    host_secrets: Arc<std::sync::OnceLock<Vec<String>>>,
    host_source: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>>,
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

/// A streaming redactor: always holds back the last `longest_token - 1` bytes between chunks,
/// whatever they are; [`StreamRedactor::finish`] flushes them at EOF. The hold-back is
/// content-independent on purpose (see [`StreamRedactor::push`]).
struct StreamRedactor<'a> {
    tokens: Vec<&'a [u8]>,
    pending: Vec<u8>,
}

impl<'a> StreamRedactor<'a> {
    fn new(token: &'a str) -> Self {
        Self::many(&[token])
    }

    /// Several secrets at once; the longest is tried first at every position.
    fn many(tokens: &[&'a str]) -> Self {
        let mut tokens: Vec<&[u8]> = tokens
            .iter()
            .map(|t| t.as_bytes())
            .filter(|t| !t.is_empty())
            .collect();
        tokens.sort_by_key(|t| std::cmp::Reverse(t.len()));
        StreamRedactor {
            tokens,
            pending: Vec::new(),
        }
    }

    /// Feed a chunk; returns the bytes now safe to show.
    ///
    /// The hold-back does not depend on the content. Holding back only a tail that *is* a prefix
    /// of a secret would be a prefix oracle: with no OS sandbox and `--offline`, a command can
    /// print a guess and sleep, and a job reading the live stream before EOF learns from the byte
    /// count whether the guess is a true prefix of the secret. Here the number of bytes shown
    /// after `n` bytes arrived depends only on `n`.
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let longest = self.tokens.iter().map(|t| t.len()).max().unwrap_or(0);
        let hold = longest.saturating_sub(1);
        let Some(emit) = self.pending.len().checked_sub(hold).filter(|e| *e > 0) else {
            return Vec::new();
        };
        // Any secret that starts before `emit` ends inside `pending` (start + len <=
        // emit - 1 + hold + 1 = pending.len()), so every position decided here is decided on
        // full data.
        let buf = &self.pending;
        let mut out = Vec::with_capacity(emit);
        let mut i = 0;
        while i < emit {
            if let Some(t) = self.tokens.iter().find(|t| buf[i..].starts_with(t)) {
                out.extend_from_slice(REDACTED.as_bytes());
                i += t.len();
            } else {
                out.push(buf[i]);
                i += 1;
            }
        }
        self.pending.drain(..i);
        out
    }

    /// EOF: the held-back tail is shorter than the longest token, so it cannot hold that one, but
    /// it can hold a shorter token whole. Mask every token in it, longest first as in
    /// [`StreamRedactor::many`], and flush.
    fn finish(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.pending);
        for t in &self.tokens {
            if !t.is_empty() && out.windows(t.len()).any(|window| window == *t) {
                out = redact(&out, &String::from_utf8_lossy(t));
            }
        }
        out
    }
}

impl NetSandbox {
    /// `setting_on` = [`sandbox_net_setting`]; `offline` = the hard switch.
    pub fn new(inner: Arc<dyn Sandbox>, setting_on: bool, offline: bool) -> NetSandbox {
        NetSandbox {
            inner,
            net: setting_on && !offline,
            gh_token: Arc::new(std::sync::OnceLock::new()),
            gh_source: None,
            host_secrets: Arc::new(std::sync::OnceLock::new()),
            host_source: None,
        }
    }

    /// The credentials to scrub from the output of commands that run with the user's own
    /// rights (no OS sandbox), whatever the bash network setting: `source` runs lazily, once,
    /// at the first such command, and returns every GitHub CLI token on the machine (all
    /// accounts, all hosts), so `gh auth token --user other` or a read of the token file is
    /// scrubbed too.
    pub fn with_host_secrets_lazy(
        mut self,
        source: impl Fn() -> Vec<String> + Send + Sync + 'static,
    ) -> NetSandbox {
        self.host_source = Some(Arc::new(source));
        self
    }

    /// #52: the GitHub token (from `gh auth token`, read outside the sandbox) to pass to
    /// commands that have the network, as `GH_TOKEN` with `GH_HOST=github.com`. Ignored when the
    /// network is off (offline, `oxa.sandbox.net: off`), empty or containing whitespace. The
    /// token is only in the child's environment; it is replaced in the output of the command
    /// and never shown in [`Sandbox::bwrap_args`].
    pub fn with_gh_token(mut self, token: Option<String>) -> NetSandbox {
        let net = self.net;
        self.gh_token = Arc::new(std::sync::OnceLock::new());
        let _ = self.gh_token.set(Self::clean(net, token));
        self
    }

    /// Like [`with_gh_token`](Self::with_gh_token), but `source` runs lazily, once, at the
    /// first command that has the network, so startup never waits for it.
    pub fn with_gh_token_lazy(
        mut self,
        source: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> NetSandbox {
        self.gh_source = Some(Arc::new(source));
        self
    }

    fn clean(net: bool, token: Option<String>) -> Option<String> {
        token
            .map(|t| t.trim().to_string())
            .filter(|t| net && !t.is_empty() && !t.chars().any(char::is_whitespace))
    }

    /// The token only for a networked command that mentions `gh` or `git` as a word (#210).
    /// `git` is included because git over HTTPS authenticates through `gh auth git-credential`,
    /// which needs the same token: without it push, fetch, pull and clone would lose their
    /// credentials in the sandbox.
    fn token_for(&self, s: &ExecSpec) -> Option<&String> {
        if !s.net || !mentions_gh(&s.command) {
            return None;
        }
        self.token()
    }

    fn token(&self) -> Option<&String> {
        self.gh_token
            .get_or_init(|| {
                let src = self.gh_source.as_ref()?;
                Self::clean(self.net, src())
            })
            .as_ref()
    }

    /// The spec for a real run: the clamp plus the token, when the command has the network.
    fn clamp_run(&self, s: &ExecSpec) -> ExecSpec {
        let mut s = self.clamp(s);
        if let (true, Some(t)) = (s.net, self.token_for(&s)) {
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
        // Injection is per command, but redaction covers every networked command once the
        // token is known, so `cat token.txt` after `gh auth token > token.txt` is scrubbed.
        // `self.token()` loads a lazy token here too: the first networked command may be
        // `cat token.txt` from an earlier session.
        self.redact_output(s.net, live, |live| run(&s, live))
    }

    /// Scrub command output using the session's GitHub token. Host execution keeps its
    /// own cleared environment and never receives a token through this method.
    pub(crate) fn redact_output(
        &self,
        networked: bool,
        live: &mut dyn FnMut(&[u8]),
        run: impl FnOnce(&mut dyn FnMut(&[u8])) -> ExecResult,
    ) -> ExecResult {
        let Some(t) = (networked && self.net).then(|| self.token()).flatten() else {
            return run(live);
        };
        let mut sr = StreamRedactor::new(t);
        let mut r = run(&mut |b: &[u8]| {
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

    /// The secrets to scrub on a host-rights path: the session token (when the network is on)
    /// and every GitHub CLI credential, found once. Never empty-handed for a missing `gh`.
    fn host_redaction_secrets(&self) -> Vec<String> {
        let mut all: Vec<String> = self.token().cloned().into_iter().collect();
        let found = self.host_secrets.get_or_init(|| match &self.host_source {
            Some(source) => source(),
            None => Vec::new(),
        });
        for t in found {
            let t = t.trim();
            if !t.is_empty() && !all.iter().any(|x| x == t) {
                all.push(t.to_string());
            }
        }
        all
    }

    /// [`redact_output`](Self::redact_output) for commands that run with the user's rights:
    /// scrubs every GitHub credential, whether or not the command has the network (a file-backed
    /// token reads the same with the network off). The environment stays cleared; nothing is
    /// injected.
    pub(crate) fn redact_host_output(
        &self,
        live: &mut dyn FnMut(&[u8]),
        run: impl FnOnce(&mut dyn FnMut(&[u8])) -> ExecResult,
    ) -> ExecResult {
        let secrets = self.host_redaction_secrets();
        if secrets.is_empty() {
            return run(live);
        }
        let refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let mut sr = StreamRedactor::many(&refs);
        let mut r = run(&mut |b: &[u8]| {
            let o = sr.push(b);
            if !o.is_empty() {
                live(&o);
            }
        });
        let tail = sr.finish();
        if !tail.is_empty() {
            live(&tail);
        }
        for t in &refs {
            r.output = redact(&r.output, t);
        }
        r
    }

    fn clamp(&self, s: &ExecSpec) -> ExecSpec {
        let mut s = s.clone();
        s.net = s.net && self.net;
        s
    }
}

impl Sandbox for NetSandbox {
    fn home(&self) -> Option<&std::path::Path> {
        self.inner.home()
    }
    fn persistence_notice(&self) -> Option<&str> {
        self.inner.persistence_notice()
    }
    fn scratch_dir(&self) -> Option<&std::path::Path> {
        self.inner.scratch_dir()
    }
    fn preserves_host_path(&self, path: &std::path::Path) -> bool {
        self.inner.preserves_host_path(path)
    }
    fn for_session(&self) -> Option<Arc<dyn Sandbox>> {
        Some(Arc::new(NetSandbox {
            inner: self.inner.for_session()?,
            net: self.net,
            gh_token: self.gh_token.clone(),
            gh_source: self.gh_source.clone(),
            host_secrets: self.host_secrets.clone(),
            host_source: self.host_source.clone(),
        }))
    }
    fn check(&self) -> Result<(), OxaError> {
        self.inner.check()
    }
    fn network(&self) -> bool {
        self.net
    }
    fn network_enforced(&self) -> bool {
        self.inner.network_enforced()
    }
    fn bwrap_args(&self, s: &ExecSpec) -> Vec<String> {
        self.inner.bwrap_args(&self.clamp(s))
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.redacted_run(s, live, |s, live| self.inner.run(s, c, live))
    }
    fn run_streaming(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.redacted_run(s, live, |s, live| self.inner.run_streaming(s, c, live))
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

/// True when `gh` or `git` appears as a command word (not inside `ghost` or `a-gh`).
/// Best-effort exposure limiting, not a security control: `echo gh; printenv GH_TOKEN` still
/// matches. The real protection is output redaction, which covers every networked command.
fn mentions_gh(cmd: &str) -> bool {
    let ok = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.');
    ["gh", "git"].iter().any(|w| {
        cmd.match_indices(w).any(|(i, _)| {
            let before = cmd[..i].chars().next_back();
            let after = cmd[i + w.len()..].chars().next();
            before.is_none_or(|c| !ok(c)) && after.is_none_or(|c| !ok(c))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn lazy_gh_token_is_not_read_until_a_networked_run() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let inner: Arc<dyn Sandbox> = Arc::new(Echo(Mutex::new(vec![])));
        let n = NetSandbox::new(inner, true, false).with_gh_token_lazy(move || {
            c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some("SYNTH_LAZY_TOKEN".into())
        });
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let _ = n.bwrap_args(&spec(true));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        for _ in 0..2 {
            n.run(&spec(true), &CancelToken::new(), &mut |_| {});
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// #457: a session's copy wraps the inner sandbox's own session sandbox, keeps the network
    /// decision, and shares the token: one lookup per process, and a later session still
    /// redacts it.
    #[test]
    fn session_copy_wraps_the_inner_session_and_shares_the_token() {
        struct PerSession(Arc<Echo>);
        impl Sandbox for PerSession {
            fn check(&self) -> Result<(), OxaError> {
                Ok(())
            }
            fn for_session(&self) -> Option<Arc<dyn Sandbox>> {
                Some(self.0.clone())
            }
            fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
                vec!["process".into()]
            }
            fn run(&self, _s: &ExecSpec, _c: &CancelToken, _l: &mut dyn FnMut(&[u8])) -> ExecResult {
                panic!("a session must run in its own sandbox")
            }
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let echo = Arc::new(Echo(Mutex::new(Vec::new())));
        let n =
            NetSandbox::new(Arc::new(PerSession(echo.clone())), true, false).with_gh_token_lazy(move || {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some("SYNTH_SESSION_TOKEN".into())
            });
        for _ in 0..2 {
            let s = n.for_session().expect("a session sandbox");
            assert!(s.network());
            let r = s.run(&spec(true), &CancelToken::new(), &mut |_| {});
            let out = String::from_utf8(r.output).unwrap();
            assert!(!out.contains("SYNTH_SESSION_TOKEN"), "{out}");
            assert_eq!(echo.0.lock().unwrap().len(), 1);
            echo.0.lock().unwrap().clear();
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        // An inner sandbox without session state: no copy, the process sandbox is shared.
        let plain = NetSandbox::new(Arc::new(Echo(Mutex::new(Vec::new()))), true, false);
        assert!(plain.for_session().is_none());
    }

    #[test]
    fn git_gets_token_but_lookalikes_do_not_and_output_stays_redacted() {
        let inner = Arc::new(Echo(Mutex::new(Vec::new())));
        let sb = NetSandbox::new(inner.clone(), true, false).with_gh_token(Some("ghp_SECRET123".into()));
        let go = |cmd: &str| {
            let mut sp = spec(true);
            sp.command = cmd.into();
            let r = sb.run(&sp, &CancelToken::new(), &mut |_| {});
            let env = inner.0.lock().unwrap().pop().unwrap();
            (
                env.iter().any(|(k, _)| k == "GH_TOKEN"),
                String::from_utf8(r.output).unwrap(),
            )
        };
        assert!(go("git push origin x").0);
        assert!(!go("npm install gitignore-parser").0);
        let (inj, out) = go("curl https://example.invalid");
        assert!(!inj);
        assert!(!out.contains("ghp_SECRET123"));
        // Echo only prints a token it was given; check redaction directly on a leaked value.
        struct Leak;
        impl Sandbox for Leak {
            fn check(&self) -> Result<(), OxaError> {
                Ok(())
            }
            fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
                Vec::new()
            }
            fn run(&self, _s: &ExecSpec, _c: &CancelToken, l: &mut dyn FnMut(&[u8])) -> ExecResult {
                l(b"ghp_SECRET123");
                ExecResult {
                    output: b"ghp_SECRET123".to_vec(),
                    exit: Some(0),
                    timed_out: false,
                    unsandboxed: false,
                }
            }
        }
        let sb = NetSandbox::new(Arc::new(Leak), true, false).with_gh_token(Some("ghp_SECRET123".into()));
        let mut sp = spec(true);
        sp.command = "cat token.txt".into();
        let mut live = Vec::new();
        let r = sb.run(&sp, &CancelToken::new(), &mut |b| live.extend_from_slice(b));
        assert_eq!(String::from_utf8(r.output).unwrap(), "[redacted]");
        assert_eq!(String::from_utf8(live).unwrap(), "[redacted]");
    }

    #[test]
    fn lazy_token_is_redacted_from_a_first_non_gh_networked_command() {
        struct Leak;
        impl Sandbox for Leak {
            fn check(&self) -> Result<(), OxaError> {
                Ok(())
            }
            fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
                Vec::new()
            }
            fn run(&self, _s: &ExecSpec, _c: &CancelToken, l: &mut dyn FnMut(&[u8])) -> ExecResult {
                l(b"ghp_LAZY123");
                ExecResult {
                    output: b"ghp_LAZY123".to_vec(),
                    exit: Some(0),
                    timed_out: false,
                    unsandboxed: false,
                }
            }
        }
        let sb =
            NetSandbox::new(Arc::new(Leak), true, false).with_gh_token_lazy(|| Some("ghp_LAZY123".into()));
        let mut sp = spec(true);
        sp.command = "cat token.txt".into();
        let mut live = Vec::new();
        let r = sb.run(&sp, &CancelToken::new(), &mut |b| live.extend_from_slice(b));
        assert_eq!(String::from_utf8(r.output).unwrap(), "[redacted]");
        assert_eq!(String::from_utf8(live).unwrap(), "[redacted]");
    }

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
                unsandboxed: false,
            }
        }
    }

    /// Host-rights output is scrubbed of every GitHub credential with the network off too: a
    /// file-backed token reads the same offline, and another account's token is a secret too.
    #[test]
    fn host_output_redacts_every_account_with_the_network_off() {
        let inner: Arc<dyn Sandbox> = Arc::new(Echo(Mutex::new(vec![])));
        let n = NetSandbox::new(inner, false, true)
            .with_host_secrets_lazy(|| vec!["SYNTH_ACCOUNT_A".into(), "SYNTH_ACCOUNT_B".into()]);
        let mut live = Vec::new();
        let r = n.redact_host_output(&mut |b| live.extend_from_slice(b), |live| {
            live(b"a=SYNTH_ACCOUNT_A b=SYNTH_ACC");
            live(b"OUNT_B end");
            ExecResult {
                output: b"a=SYNTH_ACCOUNT_A b=SYNTH_ACCOUNT_B end".to_vec(),
                exit: Some(0),
                timed_out: false,
                unsandboxed: false,
            }
        });
        let live = String::from_utf8(live).unwrap();
        assert_eq!(live, "a=[redacted] b=[redacted] end");
        assert_eq!(
            String::from_utf8(r.output).unwrap(),
            "a=[redacted] b=[redacted] end"
        );
    }

    /// Streaming like a background job: records env, emits the token in the live stream only.
    struct Stream(Mutex<Vec<Vec<(String, String)>>>, usize);
    impl Sandbox for Stream {
        fn check(&self) -> Result<(), OxaError> {
            Ok(())
        }
        fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, _s: &ExecSpec, _c: &CancelToken, _l: &mut dyn FnMut(&[u8])) -> ExecResult {
            unreachable!("background jobs stream")
        }
        fn run_streaming(&self, s: &ExecSpec, _c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
            self.0.lock().unwrap().push(s.env.clone());
            let out = b"x SYNTH_BG_TOKEN_9 y";
            live(&out[..self.1]);
            live(&out[self.1..]);
            ExecResult {
                output: Vec::new(),
                exit: Some(0),
                timed_out: false,
                unsandboxed: false,
            }
        }
    }

    /// Codex security review on #1563 (follow-up #1808): the live output must not be a prefix
    /// oracle. Two streams of the same length that differ only in whether they share a prefix
    /// with the token show exactly the same number of bytes after every chunk, so a job reading
    /// the stream before EOF learns nothing, and a token split across chunks is still masked.
    #[test]
    fn stream_hold_back_is_independent_of_token_prefixes() {
        const T: &str = "SYNTH_BG_TOKEN_9";
        fn per_push(chunks: &[&str]) -> (Vec<usize>, String) {
            let mut sr = StreamRedactor::new(T);
            let mut lens = Vec::new();
            let mut all = Vec::new();
            for c in chunks {
                let o = sr.push(c.as_bytes());
                lens.push(o.len());
                all.extend_from_slice(&o);
            }
            let tail = sr.finish();
            lens.push(tail.len());
            all.extend_from_slice(&tail);
            (lens, String::from_utf8(all).unwrap())
        }
        let probes: &[(&[&str], &[&str])] = &[
            // A true prefix vs a wrong guess of the same length, then the job "sleeps".
            (&["x ", "SYNTH_BG"], &["x ", "QYNTH_BG"]),
            (&["SYNTH_BG_TOKEN_"], &["QYNTH_BG_TOKEN_"]),
            (&["S"], &["Q"]),
            (
                &["abc", "SYNTH", "_BG_T", "OKEN_", "X done"],
                &["abc", "SYNTZ", "_BG_T", "OKEN_", "X done"],
            ),
            (
                &["0123456789abcdefSYNTH_BG_TOKEN_"],
                &["0123456789abcdefQQQQQQQQQQQQQQQ"],
            ),
        ];
        for (a, b) in probes {
            let (la, oa) = per_push(a);
            let (lb, ob) = per_push(b);
            assert_eq!(
                la, lb,
                "shown bytes per chunk depend on the token: {a:?} vs {b:?}"
            );
            assert!(!oa.contains(T) && !ob.contains(T));
            assert_eq!(oa.len(), a.concat().len(), "no full token, nothing replaced");
        }
        // Past the hold-back a full token is masked wherever the chunks split it, and several
        // secrets share one content-independent hold-back sized by the longest.
        let full = "pre SYNTH_BG_TOKEN_9 mid SYNTH_BG_TOKEN_9 post";
        for at in 0..=full.len() {
            let (head, tail) = full.split_at(at);
            let mut sr = StreamRedactor::many(&[T, "SHORT_SECRET"]);
            let mut shown = sr.push(head.as_bytes());
            shown.extend_from_slice(&sr.push(tail.as_bytes()));
            shown.extend_from_slice(&sr.finish());
            let shown = String::from_utf8(shown).unwrap();
            assert_eq!(shown, "pre [redacted] mid [redacted] post", "split at {at}");
        }
    }

    fn stream(setting: bool, offline: bool, at: usize) -> (Vec<(String, String)>, String, ExecResult) {
        let inner = Arc::new(Stream(Mutex::new(Vec::new()), at));
        let sb =
            NetSandbox::new(inner.clone(), setting, offline).with_gh_token(Some("SYNTH_BG_TOKEN_9".into()));
        let mut live = Vec::new();
        let r = sb.run_streaming(&spec(true), &CancelToken::new(), &mut |b| {
            live.extend_from_slice(b)
        });
        let env = inner.0.lock().unwrap().pop().unwrap();
        (env, String::from_utf8(live).unwrap(), r)
    }

    #[test]
    fn streaming_injects_auth_and_redacts_split_chunks() {
        let total = "x SYNTH_BG_TOKEN_9 y".len();
        for at in 0..=total {
            let (env, live, r) = stream(true, false, at);
            assert!(
                env.contains(&("GH_TOKEN".into(), "SYNTH_BG_TOKEN_9".into())),
                "{env:?}"
            );
            assert!(env.contains(&("GH_HOST".into(), "github.com".into())));
            assert_eq!(live, "x [redacted] y", "split at {at}");
            assert!(r.output.is_empty());
        }
    }

    #[test]
    fn streaming_offline_refuses_token_and_network() {
        let (env, live, _) = stream(true, true, 5);
        assert!(
            !env.iter().any(|(k, _)| k == "GH_TOKEN" || k == "GH_HOST"),
            "{env:?}"
        );
        assert_eq!(live, "x SYNTH_BG_TOKEN_9 y");
    }

    fn spec(net: bool) -> ExecSpec {
        ExecSpec {
            command: "gh x".into(),
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
    fn token_only_goes_to_gh_commands() {
        let inner = Arc::new(Echo(Mutex::new(Vec::new())));
        let sb = NetSandbox::new(inner.clone(), true, false).with_gh_token(Some("ghp_SECRET123".into()));
        for (cmd, want) in [
            ("curl https://example.invalid", false),
            ("npm install ghost", false),
            ("gh pr list", true),
            ("cd x && gh api /user | jq", true),
        ] {
            let mut sp = spec(true);
            sp.command = cmd.into();
            sb.run(&sp, &CancelToken::new(), &mut |_| {});
            let env = inner.0.lock().unwrap().pop().unwrap();
            assert_eq!(env.iter().any(|(k, _)| k == "GH_TOKEN"), want, "{cmd}");
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
            ExecResult {
                output: out,
                exit: Some(0),
                timed_out: false,
                unsandboxed: false,
            }
        }
    }

    #[test]
    fn a_token_split_across_live_chunks_never_leaks() {
        let total = "a SYNTH_MARKER_TOKEN_42 b SYNTH_MARKER_TOKEN_42".len();
        for at in 0..=total {
            let sb = NetSandbox::new(Arc::new(Split(at)), true, false)
                .with_gh_token(Some("SYNTH_MARKER_TOKEN_42".into()));
            let mut live = Vec::new();
            let r = sb.run(&spec(true), &CancelToken::new(), &mut |b| {
                live.extend_from_slice(b)
            });
            let live = String::from_utf8(live).unwrap();
            assert_eq!(live, "a [redacted] b [redacted]", "split at {at}");
            assert_eq!(String::from_utf8(r.output).unwrap(), live, "split at {at}");
        }
    }
}
