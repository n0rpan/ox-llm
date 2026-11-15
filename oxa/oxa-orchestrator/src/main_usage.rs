//! In-process inference source for the exact engine used by an attached TUI.
use oxa_core::{Deps, Session};
use oxa_proto::{AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, OxaError, StreamEvent};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock, Weak},
};

type Entry = (Weak<dyn Engine>, Weak<Source>);
static REGISTRY: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
#[derive(Clone, Copy, Default)]
pub(crate) struct Snapshot {
    pub total: u64,
    pub unknown: bool,
    pub active: usize,
}
impl Snapshot {
    pub fn tokens(self) -> Option<u64> {
        (!self.unknown && self.active == 0).then_some(self.total)
    }
}
struct State {
    allowed: bool,
    scoped: bool,
    cap: Option<u64>,
    deadline: Option<u64>,
    total: u64,
    unknown: bool,
    next: u64,
    active: BTreeMap<u64, CancelToken>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            allowed: true,
            scoped: false,
            cap: None,
            deadline: None,
            total: 0,
            unknown: false,
            next: 1,
            active: BTreeMap::new(),
        }
    }
}
#[derive(Default)]
pub(crate) struct Source(Mutex<State>);
impl Source {
    /// A new goal cannot absorb or erase a still-running unscoped request.
    pub fn scope(&self) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state.active.is_empty() {
            return false;
        }
        state.scoped = true;
        state.allowed = false;
        state.total = 0;
        state.unknown = false;
        true
    }
    pub fn unscoped(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.scoped = false;
        state.allowed = true;
        state.cap = None;
        state.deadline = None;
    }
    /// Called under the session owner's admission lock, before entering Core.
    pub fn begin(&self, cap: Option<u64>, remaining_ms: u64) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state.active.is_empty() || !state.scoped || cap == Some(0) || remaining_ms == 0 {
            return false;
        }
        let Some(deadline) = crate::resume::boot_time_ns().ok().and_then(|now| {
            remaining_ms
                .checked_mul(1_000_000)
                .and_then(|n| now.checked_add(n))
        }) else {
            return false;
        };
        state.total = 0;
        state.unknown = false;
        state.cap = cap;
        state.deadline = Some(deadline);
        state.allowed = true;
        true
    }
    pub fn snapshot(&self) -> Snapshot {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Snapshot {
            total: state.total,
            unknown: state.unknown,
            active: state.active.len(),
        }
    }
    /// End admission without discarding a legitimate background return. The
    /// owner defers goal settlement until active calls drain; its deadline and
    /// explicit interrupts still cancel those calls through `close`.
    pub fn freeze(&self) -> Snapshot {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.allowed = false;
        Snapshot {
            total: state.total,
            unknown: state.unknown,
            active: state.active.len(),
        }
    }
    /// Refuse new inference and cancel every outstanding source request. Do not
    /// turn an unfinished request into known zero, or wait under the owner lock.
    pub fn close(&self) -> Snapshot {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.allowed = false;
        for cancel in state.active.values() {
            cancel.cancel();
        }
        Snapshot {
            total: state.total,
            unknown: state.unknown,
            active: state.active.len(),
        }
    }
    fn enter(&self, cancel: &CancelToken) -> Result<u64, OxaError> {
        let mut state = self.0.lock().map_err(|_| unavailable())?;
        let expired = state
            .deadline
            .is_some_and(|deadline| crate::resume::boot_time_ns().map_or(true, |now| now >= deadline));
        if !state.allowed
            || cancel.is_cancelled()
            || expired
            || state.active.len() >= 8
            || (state.scoped && state.unknown)
            || state.cap.is_some_and(|cap| state.total >= cap)
        {
            return Err(unavailable());
        }
        let id = state.next;
        state.next = id.checked_add(1).ok_or_else(unavailable)?;
        state.active.insert(id, cancel.clone());
        Ok(id)
    }
    fn end(&self, id: u64, tokens: Option<u64>) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.remove(&id).is_none() {
            state.unknown = true;
            return;
        }
        match tokens.and_then(|n| state.total.checked_add(n)) {
            Some(total) => state.total = total,
            None => state.unknown = true,
        }
    }
}
fn unavailable() -> OxaError {
    OxaError::new(
        "usage_unavailable",
        "session goal inference is paused, capped, or unsettled",
    )
}
struct Call<'a> {
    source: &'a Source,
    id: Option<u64>,
}
impl Call<'_> {
    fn finish(mut self, tokens: Option<u64>) {
        self.source.end(self.id.take().unwrap(), tokens);
    }
}
impl Drop for Call<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.source.end(id, None);
        }
    }
}
struct Meter {
    inner: Arc<dyn Engine>,
    source: Arc<Source>,
}
impl Engine for Meter {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.inner.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }
    fn count_prompt(&self, request: &ChatRequest) -> Result<Option<u64>, OxaError> {
        self.inner.count_prompt(request)
    }
    fn clear_hint(&self, hint: &str) -> Result<(), OxaError> {
        self.inner.clear_hint(hint)
    }
    fn chat(
        &self,
        request: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        let call = Call {
            source: &self.source,
            id: Some(self.source.enter(cancel)?),
        };
        let result = self.inner.chat(request, cancel, on);
        call.finish(
            result
                .as_ref()
                .ok()
                .map(|r| u64::from(r.usage.prompt) + u64::from(r.usage.completion)),
        );
        result
    }
}
pub(crate) fn augment(mut deps: Deps) -> Deps {
    if std::env::var_os("OXA_SESSION_CONTROL_DIRECTORY").is_none()
        || std::env::var("OXA_OWNED_WORKER_USAGE").as_deref() == Ok("1")
    {
        return deps;
    }
    let mut registry = REGISTRY
        .get_or_init(|| Mutex::new(vec![]))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    registry.retain(|(engine, source)| engine.strong_count() != 0 && source.strong_count() != 0);
    if registry
        .iter()
        .any(|(engine, _)| engine.upgrade().is_some_and(|e| Arc::ptr_eq(&e, &deps.engine)))
        || registry.len() >= 64
    {
        return deps;
    }
    let source = Arc::new(Source::default());
    deps.engine = Arc::new(Meter {
        inner: deps.engine,
        source: source.clone(),
    });
    registry.push((Arc::downgrade(&deps.engine), Arc::downgrade(&source)));
    deps
}
pub(crate) fn for_session(session: &Session) -> Option<Arc<Source>> {
    let registry = REGISTRY.get()?.lock().ok()?;
    registry.iter().find_map(|(engine, source)| {
        engine
            .upgrade()
            .filter(|e| Arc::ptr_eq(e, &session.inner().engine))?;
        source.upgrade()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_fake::{ScriptTurn, ScriptedEngine};
    use oxa_proto::{FrozenPrefix, Hints, TurnKind, WireMsg};
    #[test]
    fn every_kind_is_charged_and_gate_refuses_before_next_sdk_call() {
        let inner = Arc::new(ScriptedEngine::new(vec![
            ScriptTurn::text("summary"),
            ScriptTurn::text("done"),
            ScriptTurn::text("cap crossed"),
        ]));
        let source = Arc::new(Source::default());
        let meter = Meter {
            inner: inner.clone(),
            source: source.clone(),
        };
        assert!(source.scope());
        assert!(source.begin(Some(100000), 10000));
        let prefix = FrozenPrefix {
            sections: vec![],
            tools_json: "[]".into(),
            sha256: [0; 32],
        };
        let messages = vec![WireMsg::user("source-only usage")];
        let mut total = 0;
        for kind in [TurnKind::Prefill, TurnKind::Compaction, TurnKind::Chat] {
            let request = ChatRequest {
                prefix: &prefix,
                messages: &messages,
                model: "fake-model",
                max_tokens: if kind == TurnKind::Prefill { 0 } else { 128 },
                thinking: false,
                preserve_thinking: false,
                sampling: None,
                hints: Hints {
                    session: "main",
                    idem_key: "",
                    kind,
                    background: kind != TurnKind::Chat,
                    snapshot_keep: false,
                },
            };
            let result = meter.chat(&request, &CancelToken::new(), &mut |_| {}).unwrap();
            total += u64::from(result.usage.prompt) + u64::from(result.usage.completion);
            if kind == TurnKind::Chat {
                assert_eq!(source.close().tokens(), Some(total));
                assert!(meter.chat(&request, &CancelToken::new(), &mut |_| {}).is_err());
                assert_eq!(inner.requests().len(), 3);
                assert!(source.begin(Some(1), 10000));
                meter.chat(&request, &CancelToken::new(), &mut |_| {}).unwrap();
                assert!(meter.chat(&request, &CancelToken::new(), &mut |_| {}).is_err());
                assert_eq!(inner.requests().len(), 4);
            }
        }
        meter.clear_hint("main").unwrap();
        assert_eq!(inner.clears(), vec!["main"]);
    }
    #[test]
    fn pending_failed_and_expired_inference_are_never_known_zero() {
        let source = Source::default();
        let cancel = CancelToken::new();
        let id = source.enter(&cancel).unwrap();
        assert!(!source.scope(), "cannot reset a request into a different goal");
        assert!(source.close().tokens().is_none());
        assert!(cancel.is_cancelled());
        source.end(id, None);
        assert!(source.scope());
        assert!(source.begin(Some(1000), 10000));
        let id = source.enter(&CancelToken::new()).unwrap();
        source.end(id, None);
        assert!(source.snapshot().tokens().is_none());
        assert!(source.enter(&CancelToken::new()).is_err());
        source.close();
        assert!(
            source.begin(None, 10000),
            "time-only continuation remains available"
        );
        let id = source.enter(&CancelToken::new()).unwrap();
        source.end(id, Some(0));
        assert_eq!(source.snapshot().tokens(), Some(0));
        source.0.lock().unwrap().deadline = Some(0);
        assert!(source.enter(&CancelToken::new()).is_err());
    }
}
