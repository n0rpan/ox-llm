//! O-167(b) (wave H): the bash network as a sandbox property.
//!
//! The agent's tools get the internet by default; the harness itself never phones home.
//! [`NetSandbox`] wraps the process sandbox and answers [`Sandbox::network`] from the setting
//! `oxa.sandbox.net` (`on`, the default, or `off`) and the offline switch (`OXA_OFFLINE=1`,
//! `--offline`, `oxa.offline`: the network is off whatever the setting). It also clamps every
//! spec it runs, so with the network off no argv carries `--share-net`, whoever built the spec
//! (an extension's `exec` with `net: true` included).
use oxa_proto::{CancelToken, ExecResult, ExecSpec, OxaError, Sandbox};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// Session copies share the current credential and the redaction history.
    gh_token: Arc<Mutex<TokenState>>,
    gh_source: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    gh_ttl: Option<Duration>,
    /// A configured source is for redaction only, never for shell authentication.
    inject_gh_token: bool,
    /// Every credential the owner's GitHub CLI holds (all accounts and hosts), found lazily
    /// once for output redaction on host-rights paths. Unlike `gh_token` this does not depend
    /// on the bash network: a token file is readable with the network off too.
    host_secrets: Arc<std::sync::OnceLock<Vec<String>>>,
    host_source: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>>,
}

#[derive(Default)]
struct TokenState {
    loaded: Option<Instant>,
    current: Option<Arc<str>>,
    seen: Vec<Arc<str>>,
}

/// The replacement for a redacted token in tool output.
const REDACTED: &str = "[redacted]";

#[derive(Default)]
struct CredentialRejection {
    tail: Vec<u8>,
    detected: bool,
}

impl CredentialRejection {
    fn observe(&mut self, bytes: &[u8]) {
        const MESSAGES: &[&[u8]] = &[b"HTTP 401", b"Bad credentials", b"Invalid username or token"];
        const OVERLAP: usize = b"Invalid username or token".len() - 1;
        if self.detected {
            return;
        }
        let mut boundary = std::mem::take(&mut self.tail);
        boundary.extend_from_slice(&bytes[..bytes.len().min(OVERLAP)]);
        self.detected = MESSAGES.iter().any(|message| {
            [bytes, boundary.as_slice()]
                .iter()
                .any(|part| part.windows(message.len()).any(|window| window == *message))
        });
        let last = if bytes.len() >= OVERLAP { bytes } else { &boundary };
        self.tail
            .extend_from_slice(&last[last.len().saturating_sub(OVERLAP)..]);
    }
}

/// A streaming redactor: always holds back the last `longest_token - 1` bytes between chunks,
/// whatever they are; [`StreamRedactor::finish`] flushes them at EOF. The hold-back is
/// content-independent on purpose (see [`StreamRedactor::push`]).
struct StreamRedactor {
    tokens: Vec<Arc<str>>,
    pending: Vec<u8>,
    seen: usize,
}

impl StreamRedactor {
    #[cfg(test)]
    fn new(token: &str) -> Self {
        Self::many([token])
    }

    /// Several secrets at once; the longest is tried first at every position.
    fn many<'a>(tokens: impl IntoIterator<Item = &'a str>) -> Self {
        let mut tokens: Vec<Arc<str>> = tokens
            .into_iter()
            .filter(|t| !t.is_empty())
            .map(Arc::from)
            .collect();
        tokens.sort_by_key(|t| std::cmp::Reverse(t.len()));
        StreamRedactor {
            tokens,
            pending: Vec::new(),
            seen: 0,
        }
    }

    fn include_session_tokens(&mut self, controls: Option<&NetSandbox>) {
        if let Some(net) = controls {
            let state = net.token_state();
            if state.seen.len() > self.seen {
                self.tokens.extend(state.seen[self.seen..].iter().cloned());
                self.tokens.sort_by_key(|t| std::cmp::Reverse(t.len()));
                self.seen = state.seen.len();
            }
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
        let (out, consumed) = self.mask(&self.pending, emit);
        self.pending.drain(..consumed);
        out
    }

    /// EOF: the held-back tail may contain a shorter secret. Mask it before sending it.
    fn finish(&mut self) -> Vec<u8> {
        let pending = std::mem::take(&mut self.pending);
        self.mask(&pending, pending.len()).0
    }

    fn mask(&self, bytes: &[u8], emit: usize) -> (Vec<u8>, usize) {
        let mut out = Vec::with_capacity(emit);
        let mut i = 0;
        while i < emit {
            if let Some(t) = self.tokens.iter().find(|t| bytes[i..].starts_with(t.as_bytes())) {
                out.extend_from_slice(REDACTED.as_bytes());
                i += t.len();
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        (out, i)
    }
}

impl NetSandbox {
    /// `setting_on` = [`sandbox_net_setting`]; `offline` = the hard switch.
    pub fn new(inner: Arc<dyn Sandbox>, setting_on: bool, offline: bool) -> NetSandbox {
        NetSandbox {
            inner,
            net: setting_on && !offline,
            gh_token: Arc::new(Mutex::default()),
            gh_source: None,
            gh_ttl: None,
            inject_gh_token: true,
            host_secrets: Arc::new(std::sync::OnceLock::new()),
            host_source: None,
        }
    }

    /// The credentials to scrub from the output of commands that run with the user's own
    /// rights (no OS sandbox), whatever the bash network setting: `source` runs lazily, once,
    /// at the first such command. The session supplies its known values, including GitHub
    /// CLI tokens for every account and host.
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
        let current = Self::clean(self.net, token).map(Arc::from);
        self.gh_token = Arc::new(Mutex::new(TokenState {
            seen: current.iter().cloned().collect(),
            current,
            loaded: Some(Instant::now()),
        }));
        self.gh_source = None;
        self.gh_ttl = None;
        self
    }

    /// Read the source lazily, once, at the first command that needs it.
    pub fn with_gh_token_lazy(
        mut self,
        source: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> NetSandbox {
        self.gh_token = Arc::new(Mutex::default());
        self.gh_source = Some(Arc::new(source));
        self.gh_ttl = None;
        self
    }

    /// Like [`Self::with_gh_token_lazy`], but read again before use once `ttl` elapses
    /// or GitHub rejects the credential. Never replay the command. A failed refresh
    /// supplies no token, but every token previously supplied stays redacted.
    pub fn with_gh_token_refreshing(
        self,
        source: impl Fn() -> Option<String> + Send + Sync + 'static,
        ttl: Duration,
    ) -> NetSandbox {
        let mut this = self.with_gh_token_lazy(source);
        this.gh_ttl = Some(ttl);
        this
    }

    /// Refresh a known credential for output masking, but never add it to a command environment.
    pub fn with_gh_token_redaction_refreshing(
        self,
        source: impl Fn() -> Option<String> + Send + Sync + 'static,
        ttl: Duration,
    ) -> NetSandbox {
        let mut this = self.with_gh_token_refreshing(source, ttl);
        this.inject_gh_token = false;
        this
    }

    fn clean(net: bool, token: Option<String>) -> Option<String> {
        token
            .map(|t| t.trim().to_string())
            .filter(|t| net && !t.is_empty() && !t.chars().any(|ch| ch.is_whitespace() || ch.is_control()))
    }

    fn token_state(&self) -> std::sync::MutexGuard<'_, TokenState> {
        self.gh_token.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Look up and register a token before handing it to an executor. Serialize refreshes
    /// across sessions so a newly minted credential is also known to running redactors.
    fn token(&self) -> Option<Arc<str>> {
        if !self.net {
            return None;
        }
        let mut state = self.token_state();
        if let Some(source) = &self.gh_source {
            let fresh = state
                .loaded
                .is_some_and(|at| self.gh_ttl.is_none_or(|ttl| at.elapsed() < ttl));
            if !fresh {
                state.current = Self::clean(true, source()).map(Arc::from);
                state.loaded = Some(Instant::now());
                if let Some(token) = state.current.clone().filter(|t| !state.seen.contains(t)) {
                    state.seen.push(token);
                }
            }
        }
        state.current.clone()
    }

    fn redacted_run(
        &self,
        spec: &ExecSpec,
        live: &mut dyn FnMut(&[u8]),
        run: impl FnOnce(&ExecSpec, &mut dyn FnMut(&[u8])) -> ExecResult,
    ) -> ExecResult {
        let mut s = self.clamp(spec);
        let host = self.inner.host_rights() || !self.inner.network_enforced();
        // Even a non-GitHub command may print a previously saved credential.
        let token = (s.net || host).then(|| self.token()).flatten();
        // The legacy stored login supports Git over HTTPS, including the GitHub CLI credential
        // helper. A configured credential is only for masking; command text cannot authorize it.
        let token = token.filter(|_| self.inject_gh_token && s.net && mentions_gh(&s.command));
        if self.gh_ttl.is_some() || token.is_some() {
            s.env.retain(|(k, _)| k != "GH_TOKEN" && k != "GITHUB_TOKEN");
        }
        if let Some(t) = &token {
            s.env.retain(|(k, _)| k != "GH_HOST");
            s.env.push(("GH_TOKEN".into(), t.to_string()));
            s.env.push(("GH_HOST".into(), "github.com".into()));
        }
        let mut rejection = CredentialRejection::default();
        let watch = self.gh_ttl.is_some() && token.is_some();
        let invoke = |live: &mut dyn FnMut(&[u8])| {
            if !watch {
                return run(&s, live);
            }
            let result = run(&s, &mut |bytes| {
                rejection.observe(bytes);
                live(bytes);
            });
            rejection.observe(&result.output);
            result
        };
        let result = if host {
            Self::redact_host_known(Some(self), &s.env, live, invoke)
        } else {
            Self::redact_output(Some(self), std::iter::empty(), live, invoke)
        };
        if rejection.detected && result.exit.is_some_and(|code| code != 0) {
            let mut state = self.token_state();
            // A slow command must not invalidate another session's newer credential.
            if state
                .current
                .as_ref()
                .zip(token.as_ref())
                .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
            {
                state.loaded = None;
            }
        }
        result
    }

    /// Host shells mask known credentials even when the network is off. This never injects
    /// a value. A configured source is read before the first command that could print it.
    pub(crate) fn redact_host_output(
        controls: Option<&Self>,
        env: &[(String, String)],
        live: &mut dyn FnMut(&[u8]),
        run: impl FnOnce(&mut dyn FnMut(&[u8])) -> ExecResult,
    ) -> ExecResult {
        if let Some(net) = controls {
            net.token();
        }
        Self::redact_host_known(controls, env, live, run)
    }

    fn redact_host_known(
        controls: Option<&Self>,
        env: &[(String, String)],
        live: &mut dyn FnMut(&[u8]),
        run: impl FnOnce(&mut dyn FnMut(&[u8])) -> ExecResult,
    ) -> ExecResult {
        let host = controls.into_iter().flat_map(|net| {
            net.host_secrets.get_or_init(|| {
                net.host_source
                    .as_ref()
                    .map(|source| source())
                    .unwrap_or_default()
            })
        });
        let secrets = host.map(String::as_str).chain(
            env.iter()
                .filter(|(name, _)| crate::is_host_secret_name(name))
                .map(|(_, value)| value.as_str()),
        );
        Self::redact_output(controls, secrets, live, run)
    }

    /// Running commands see new credentials too. Never forget replaced credentials: files
    /// and background processes may still hold them.
    fn redact_output<'a>(
        controls: Option<&Self>,
        secrets: impl IntoIterator<Item = &'a str>,
        live: &mut dyn FnMut(&[u8]),
        run: impl FnOnce(&mut dyn FnMut(&[u8])) -> ExecResult,
    ) -> ExecResult {
        let mut sr = StreamRedactor::many(secrets);
        sr.include_session_tokens(controls);
        if sr.tokens.is_empty() && controls.is_none_or(|net| net.gh_source.is_none()) {
            return run(live);
        }
        let mut result = run(&mut |bytes| {
            sr.include_session_tokens(controls);
            let masked = sr.push(bytes);
            if !masked.is_empty() {
                live(&masked);
            }
        });
        sr.include_session_tokens(controls);
        let tail = sr.finish();
        if !tail.is_empty() {
            live(&tail);
        }
        result.output = sr.mask(&result.output, result.output.len()).0;
        result
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
    fn bash_view(
        &self,
        cwd: &std::path::Path,
        path: &std::path::Path,
        writable: bool,
    ) -> oxa_proto::BashView {
        self.inner.bash_view(cwd, path, writable)
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
            gh_ttl: self.gh_ttl,
            inject_gh_token: self.inject_gh_token,
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
    fn host_rights(&self) -> bool {
        self.inner.host_rights()
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
/// Best-effort exposure limiting for the legacy stored login, not an authorization check.
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

    #[test]
    fn redaction_only_token_is_never_in_a_github_commands_environment() {
        let echo = Arc::new(Echo(Mutex::new(Vec::new())));
        let net = NetSandbox::new(echo.clone(), true, false)
            .with_gh_token_redaction_refreshing(|| Some("SYNTH_CONFIGURED".into()), Duration::from_secs(300));
        let mut command = spec(true);
        command.command = "gh pr status".into();
        for name in ["GH_TOKEN", "GITHUB_TOKEN"] {
            command.env.push((name.into(), "SYNTH_AMBIENT".into()));
        }
        let result = net.run(&command, &CancelToken::new(), &mut |_| {});
        assert_eq!(result.exit, Some(0));
        let launched = echo.0.lock().unwrap();
        assert_eq!(launched.len(), 1);
        assert!(launched[0].iter().all(|(name, value)| {
            name != "GH_TOKEN" && name != "GITHUB_TOKEN" && value != "SYNTH_CONFIGURED"
        }));
        let result = NetSandbox::redact_output(Some(&net), [], &mut |_| {}, |_| ExecResult {
            output: b"SYNTH_CONFIGURED".to_vec(),
            ..ExecResult::default()
        });
        assert_eq!(result.output, b"[redacted]");
    }

    #[test]
    fn refresh_replaces_child_credentials_but_running_commands_mask_both() {
        let echo = Arc::new(Echo(Mutex::new(Vec::new())));
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let net = NetSandbox::new(echo.clone(), true, false).with_gh_token_refreshing(
            move || match calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) {
                0 => Some("SYNTH_FIRST_CREDENTIAL".into()),
                1 => Some("SYNTH_SECOND_CREDENTIAL".into()),
                _ => None,
            },
            Duration::ZERO,
        );
        let mut command = spec(true);
        command.command = "gh api test".into();
        command.env.push(("GITHUB_TOKEN".into(), "ambient".into()));
        let run = || net.run(&command, &CancelToken::new(), &mut |_| {});
        run();
        let mut live = Vec::new();
        let before_and_after = b"SYNTH_FIRST_CREDENTIAL SYNTH_SECOND_CREDENTIAL";
        let stream = NetSandbox::redact_output(Some(&net), [], &mut |b| live.extend_from_slice(b), |emit| {
            emit(b"SYNTH_FIRST_");
            run();
            emit(b"CREDENTIAL SYNTH_SECOND_CREDENTIAL");
            ExecResult {
                output: before_and_after.to_vec(),
                ..ExecResult::default()
            }
        });
        assert_eq!(stream.output, b"[redacted] [redacted]");
        assert_eq!(live, stream.output);
        run();
        let launched = echo.0.lock().unwrap();
        assert_eq!(launched.len(), 3);
        assert!(launched[0].contains(&("GH_TOKEN".into(), "SYNTH_FIRST_CREDENTIAL".into())));
        assert!(launched[1].contains(&("GH_TOKEN".into(), "SYNTH_SECOND_CREDENTIAL".into())));
        assert!(launched[2]
            .iter()
            .all(|(name, _)| name != "GH_TOKEN" && name != "GITHUB_TOKEN"));
    }

    #[test]
    fn a_streamed_rejection_refreshes_before_the_next_command_without_replaying() {
        struct Deny;
        impl Sandbox for Deny {
            fn check(&self) -> Result<(), OxaError> {
                Ok(())
            }
            fn bwrap_args(&self, _: &ExecSpec) -> Vec<String> {
                Vec::new()
            }
            fn run(&self, spec: &ExecSpec, _: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
                let token = spec.env.iter().find(|(name, _)| name == "GH_TOKEN").unwrap();
                assert!(!format!("{spec:?}").contains(&token.1));
                live(b"HTTP 4");
                live(b"01");
                live(token.1.as_bytes());
                ExecResult {
                    exit: Some(1),
                    ..ExecResult::default()
                }
            }
        }
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let net = NetSandbox::new(Arc::new(Deny), true, false).with_gh_token_refreshing(
            move || Some(format!("SYNTH_TOKEN_{}", c.fetch_add(1, Ordering::Relaxed))),
            Duration::from_secs(300),
        );
        let mut command = spec(true);
        command.command = "gh api test".into();
        for expected in 1..=2 {
            let mut live = Vec::new();
            let result = net.run_streaming(&command, &CancelToken::new(), &mut |b| live.extend_from_slice(b));
            assert_eq!(result.exit, Some(1));
            assert_eq!(live, b"HTTP 401[redacted]");
            assert_eq!(calls.load(Ordering::Relaxed), expected);
        }
    }

    /// #457: a session's copy wraps the inner session sandbox, keeps the network
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
        let r = NetSandbox::redact_host_output(Some(&n), &[], &mut |b| live.extend_from_slice(b), |live| {
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
            let mut sr = StreamRedactor::many([T, "SHORT_SECRET"]);
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
