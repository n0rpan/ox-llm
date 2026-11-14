//! E11: the child side of `builtin:subagent`. A subagent child is this binary started by the
//! parent's `subagent` tool (`oxa-core/src/subagent.rs`) with three environment variables:
//!
//! * `OXA_SESSION_HINT=<parent session id>.s<n>`: every chat request of the child names this
//!   session (`X-Session-Id` / `X-Oxe-Session`) instead of the child's own ephemeral id, is
//!   sent with `X-Oxe-Priority: background`, and a `chat` turn is sent as turn kind `subagent`
//!   (compaction, branch summary and prefill keep their kind). Done by wrapping the engine in
//!   [`ChildEngine`]; nothing in `oxa-core` or `oxa-engine` knows about children.
//! * `OXA_SUBAGENT_DEPTH=1`: the process is a child; `builtin:subagent` is not built (depth 1).
//! * `OXA_CHILD_SOFT_TOKENS=<n>`: the child's soft compaction limit, applied as an in-memory
//!   override of `oxa.compaction.softTokens` (the child compacts by W5 like any session).
//!
//! The variables are read from the `Env` given to `run` (never the process environment).
use oxa_config::{Env, Settings};
use oxa_core::SubagentHooks;
use oxa_proto::{
    AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, Level, OxaError, StreamEvent, TurnKind,
};
use std::sync::Arc;

/// The key the child's soft limit overrides.
const SOFT_TOKENS_KEY: &str = "oxa.compaction.softTokens";

/// What the environment says about this process as a subagent child.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChildIdentity {
    /// The session hint every request of this process carries.
    pub hint: Option<String>,
    /// `OXA_SUBAGENT_DEPTH` (0 = not a child).
    pub depth: u32,
    pub soft_tokens: Option<u32>,
}

impl ChildIdentity {
    /// Read the three variables. A malformed value is ignored with a warning in `notices`.
    pub fn from_env(env: &Env, notices: &mut Vec<(Level, String)>) -> ChildIdentity {
        let mut id = ChildIdentity::default();
        if let Some(h) = env.get(SubagentHooks::ENV_SESSION_HINT) {
            if is_child_hint(h) {
                id.hint = Some(h.to_string());
            } else {
                notices.push((
                    Level::Warning,
                    format!(
                        "{}={h} is not a child session hint (<session id>.s<n>); ignored",
                        SubagentHooks::ENV_SESSION_HINT
                    ),
                ));
            }
        }
        if let Some(d) = env.get(SubagentHooks::ENV_SUBAGENT_DEPTH) {
            match d.parse::<u32>() {
                Ok(n) => id.depth = n,
                Err(_) => {
                    // Fail closed: a garbled depth still counts as a child (no nesting).
                    id.depth = 1;
                    notices.push((
                        Level::Warning,
                        format!(
                            "{}={d} is not a number; taken as 1",
                            SubagentHooks::ENV_SUBAGENT_DEPTH
                        ),
                    ));
                }
            }
        }
        if let Some(s) = env.get(SubagentHooks::ENV_CHILD_SOFT_TOKENS) {
            match s.parse::<u32>() {
                Ok(n) if n > 0 => id.soft_tokens = Some(n),
                _ => notices.push((
                    Level::Warning,
                    format!(
                        "{}={s} is not a positive number; ignored",
                        SubagentHooks::ENV_CHILD_SOFT_TOKENS
                    ),
                )),
            }
        }
        id
    }

    /// `true` for a subagent child: it must not build `builtin:subagent` itself.
    pub fn nested(&self) -> bool {
        self.depth > 0 || self.hint.is_some()
    }

    /// The engine this process uses: [`ChildEngine`] when a hint is set.
    pub fn wrap(&self, engine: Arc<dyn Engine>) -> Arc<dyn Engine> {
        match &self.hint {
            Some(hint) => Arc::new(ChildEngine {
                inner: engine,
                hint: hint.clone(),
            }),
            None => engine,
        }
    }

    /// The settings with the child's soft compaction limit applied.
    pub fn apply(&self, settings: Settings) -> Settings {
        match self.soft_tokens {
            Some(n) => settings.with_override(SOFT_TOKENS_KEY, serde_json::Value::from(n)),
            None => settings,
        }
    }
}

/// `<id>.s<n>`, `n` decimal (the shape `HostAction::ClearEngineHint` accepts).
fn is_child_hint(h: &str) -> bool {
    h.rsplit_once(".s").is_some_and(|(id, n)| {
        !id.is_empty() && !n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit())
    })
}

/// The engine of a subagent child: every request names the child's session hint and is sent
/// in the background; a `chat` turn becomes a `subagent` turn. Everything else is forwarded.
pub(crate) struct ChildEngine {
    inner: Arc<dyn Engine>,
    hint: String,
}

impl ChildEngine {
    /// The hints of `r` as a child sends them.
    fn rewrite<'a>(&'a self, r: &ChatRequest<'a>) -> ChatRequest<'a> {
        let mut hints = r.hints;
        hints.session = &self.hint;
        hints.background = true;
        if hints.kind == TurnKind::Chat {
            hints.kind = TurnKind::Subagent;
        }
        ChatRequest { hints, ..*r }
    }
}

impl Engine for ChildEngine {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.inner.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        let req = self.rewrite(r);
        self.inner.chat(&req, cancel, on)
    }
    fn clear_hint(&self, hint: &str) -> Result<(), OxaError> {
        self.inner.clear_hint(hint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_proto::{FrozenPrefix, Hints};
    use std::sync::Mutex;

    /// Records the hints of each request; `chat` fails after recording.
    type RecCalls = Vec<(String, bool, TurnKind, bool, String)>;

    struct Rec(Mutex<RecCalls>);

    impl Engine for Rec {
        fn info(&self, _target: Option<&str>) -> Result<EngineInfo, OxaError> {
            Err(OxaError::new(oxa_proto::codes::INTERNAL, "unused"))
        }
        fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
            Ok(text.len() as u32)
        }
        fn chat(
            &self,
            r: &ChatRequest,
            _cancel: &CancelToken,
            _on: &mut dyn FnMut(StreamEvent),
        ) -> Result<AssistantTurn, OxaError> {
            let h = r.hints;
            self.0.lock().unwrap().push((
                h.session.to_string(),
                h.background,
                h.kind,
                h.snapshot_keep,
                h.idem_key.to_string(),
            ));
            Err(OxaError::new(oxa_proto::codes::INTERNAL, "recorded"))
        }
    }

    fn identity(pairs: &[(&str, &str)]) -> (ChildIdentity, Vec<(Level, String)>) {
        let mut notices = Vec::new();
        let id = ChildIdentity::from_env(&Env::from_pairs(pairs), &mut notices);
        (id, notices)
    }

    #[test]
    fn child_engine_rewrites_hints() {
        let (id, notices) = identity(&[("OXA_SESSION_HINT", "abc.s2"), ("OXA_SUBAGENT_DEPTH", "1")]);
        assert!(notices.is_empty());
        assert!(id.nested());
        let rec = Arc::new(Rec(Mutex::new(Vec::new())));
        let engine = id.wrap(rec.clone());
        assert_eq!(engine.count_tokens("four").unwrap(), 4);
        let prefix = FrozenPrefix {
            sections: Vec::new(),
            tools_json: String::new(),
            sha256: [0; 32],
        };
        let send = |kind: TurnKind, snapshot_keep: bool| {
            let r = ChatRequest {
                prefix: &prefix,
                messages: &[],
                model: "m",
                max_tokens: 1,
                thinking: false,
                preserve_thinking: false,
                hints: Hints {
                    session: "own-ephemeral-id",
                    idem_key: "k1",
                    kind,
                    background: false,
                    snapshot_keep,
                },
            };
            let _ = engine.chat(&r, &CancelToken::new(), &mut |_| {});
        };
        send(TurnKind::Chat, false);
        send(TurnKind::Compaction, false);
        send(TurnKind::Prefill, true);
        let got = rec.0.lock().unwrap().clone();
        let s = |k| ("abc.s2".to_string(), true, k);
        assert_eq!(
            got.iter().map(|g| (g.0.clone(), g.1, g.2)).collect::<Vec<_>>(),
            [
                s(TurnKind::Subagent),
                s(TurnKind::Compaction),
                s(TurnKind::Prefill)
            ]
        );
        assert!(got[2].3, "other hints unchanged");
        assert_eq!(got[0].4, "k1");
    }

    #[test]
    fn not_a_child_without_the_environment() {
        let (id, notices) = identity(&[]);
        assert_eq!(id, ChildIdentity::default());
        assert!(!id.nested() && notices.is_empty());
        let s = Settings::from_values(serde_json::Value::Null, serde_json::Value::Null);
        assert_eq!(id.apply(s.clone()), s);
    }

    #[test]
    fn soft_tokens_override_and_bad_values() {
        let (id, _) = identity(&[("OXA_CHILD_SOFT_TOKENS", "20000")]);
        let user = serde_json::json!({"oxa": {"compaction": {"softTokens": 90000}}});
        let s = id.apply(Settings::from_values(user, serde_json::Value::Null));
        assert_eq!(s.compaction().soft_tokens, Some(20000));

        let (id, notices) = identity(&[
            ("OXA_SESSION_HINT", "no-number"),
            ("OXA_SUBAGENT_DEPTH", "x"),
            ("OXA_CHILD_SOFT_TOKENS", "0"),
        ]);
        assert_eq!(id.hint, None);
        assert_eq!(id.depth, 1, "a garbled depth still forbids nesting");
        assert_eq!(id.soft_tokens, None);
        assert_eq!(notices.len(), 3);
        assert!(is_child_hint("0f3a-11.s12") && !is_child_hint(".s1") && !is_child_hint("a.s"));
    }
}
