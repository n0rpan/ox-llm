//! Owned inference accounting independent of chat/compaction presentation events.
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeSet, io};

#[derive(Default)]
pub(crate) struct Ledger {
    pub ready: bool,
    pub response_tokens: Option<u32>,
    pub refused_threshold: Option<u64>,
    last: u64,
    active: BTreeSet<u64>,
    pub unknown: bool,
    pub total: u64,
    pub seen: bool,
}
#[derive(Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum Record {
    Ready {
        version: u32,
        #[serde(default)]
        response_tokens: Option<u32>,
    },
    Refused {
        token_threshold: u64,
    },
    Start {
        id: u64,
    },
    End {
        id: u64,
        tokens: Option<u64>,
    },
}
impl Ledger {
    pub fn consume(&mut self, mut value: Value) -> io::Result<()> {
        let result = self.apply(&mut value);
        if result.is_err() {
            self.unknown = true;
        }
        result
    }
    fn apply(&mut self, value: &mut Value) -> io::Result<()> {
        value.as_object_mut().ok_or_else(invalid)?.remove("type");
        let record: Record = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        match record {
            Record::Ready {
                version: 1,
                response_tokens,
            } if !self.ready && response_tokens != Some(0) => {
                self.ready = true;
                self.response_tokens = response_tokens;
            }
            Record::Refused { token_threshold }
                if self.ready && token_threshold > 0 && self.total >= token_threshold =>
            {
                self.refused_threshold = Some(token_threshold);
            }
            Record::Start { id } if self.ready && id > self.last && self.active.len() < 8 => {
                self.last = id;
                self.active.insert(id);
            }
            Record::End { id, tokens } if self.ready && self.active.remove(&id) => {
                if let Some(tokens) = tokens {
                    self.total = self.total.checked_add(tokens).ok_or_else(invalid)?;
                    self.seen = true;
                } else {
                    self.unknown = true;
                }
            }
            _ => return Err(invalid()),
        }
        Ok(())
    }
    pub fn settled(&self) -> bool {
        self.active.is_empty() && !self.unknown
    }
    pub fn drained(&self) -> bool {
        self.active.is_empty()
    }
}
fn invalid() -> io::Error {
    io::Error::other("invalid owned inference accounting record")
}

#[cfg(feature = "interactive-control")]
mod native {
    use oxa_core::Deps;
    use oxa_proto::{AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, OxaError, StreamEvent};
    use serde_json::{json, Value};
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };
    type Emit = Arc<dyn Fn(Value) -> io::Result<()> + Send + Sync>;
    struct Counter {
        next: u64,
        observed: Option<u64>,
    }
    impl Default for Counter {
        fn default() -> Self {
            Self {
                next: 1,
                observed: Some(0),
            }
        }
    }
    /// Every admitted SDK call owns one accounting closure, even on unwind.
    struct Call<'a> {
        emit: &'a Emit,
        counter: &'a Mutex<Counter>,
        id: Option<u64>,
    }
    impl Call<'_> {
        fn finish(mut self, tokens: Option<u64>) -> io::Result<()> {
            let id = self.id.take().expect("owned inference call");
            let mut counter = self.counter.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(tokens) = tokens {
                counter.observed = counter.observed.and_then(|n| n.checked_add(tokens));
            }
            // Publish settlement under the admission lock, so a refusal cannot
            // overtake the usage record that crossed its threshold.
            (self.emit)(json!({"type":"oxa_owned_inference","phase":"end","id":id,"tokens":tokens}))
        }
    }
    impl Drop for Call<'_> {
        fn drop(&mut self) {
            if let Some(id) = self.id.take() {
                // Best effort only: a broken output cannot prove settlement to
                // the parent. Do not retry or double-panic while unwinding.
                let _ = oxa_proto::panic::contain(|| {
                    (self.emit)(json!({"type":"oxa_owned_inference","phase":"end","id":id,"tokens":null}))
                });
            }
        }
    }
    struct Meter {
        inner: Arc<dyn Engine>,
        emit: Emit,
        counter: Mutex<Counter>,
        ready: bool,
        model: Option<String>,
        response_tokens: Option<u32>,
        token_threshold: Option<u64>,
    }
    impl Engine for Meter {
        fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
            let mut info = self.inner.info(target)?;
            if let Some(cap) = self.response_tokens {
                info.max_output = info.max_output.min(cap);
            }
            Ok(info)
        }
        fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
            self.inner.count_tokens(text)
        }
        fn count_prompt(&self, r: &ChatRequest) -> Result<Option<u64>, OxaError> {
            self.inner.count_prompt(r)
        }
        fn clear_hint(&self, hint: &str) -> Result<(), OxaError> {
            self.inner.clear_hint(hint)
        }
        fn chat(
            &self,
            r: &ChatRequest,
            cancel: &CancelToken,
            on: &mut dyn FnMut(StreamEvent),
        ) -> Result<AssistantTurn, OxaError> {
            let failure = || OxaError::new("usage_unavailable", "owned inference accounting unavailable");
            if !self.ready {
                return Err(failure());
            }
            if self.model.as_deref() != Some(r.model) {
                return Err(OxaError::new(
                    "policy_violation",
                    "owned worker inference model changed",
                ));
            }
            let id = {
                let mut counter = self.counter.lock().map_err(|_| failure())?;
                let observed = counter.observed.ok_or_else(failure)?;
                if let Some(threshold) = self.token_threshold.filter(|cap| observed >= *cap) {
                    (self.emit)(
                        json!({"type":"oxa_owned_inference","phase":"refused","token_threshold":threshold}),
                    )
                    .map_err(|_| failure())?;
                    return Err(OxaError::new(
                        "token_budget",
                        "owned worker observed token threshold reached",
                    ));
                }
                let id = counter.next;
                counter.next = id.checked_add(1).ok_or_else(failure)?;
                (self.emit)(json!({"type":"oxa_owned_inference","phase":"start","id":id}))
                    .map_err(|_| failure())?;
                id
            };
            let call = Call {
                emit: &self.emit,
                counter: &self.counter,
                id: Some(id),
            };
            let mut bounded = *r;
            if let Some(cap) = self.response_tokens {
                bounded.max_tokens = bounded.max_tokens.min(cap);
            }
            let result = self.inner.chat(&bounded, cancel, on);
            let tokens = result
                .as_ref()
                .ok()
                .map(|turn| u64::from(turn.usage.prompt) + u64::from(turn.usage.completion));
            call.finish(tokens).map_err(|_| failure())?;
            result
        }
    }
    pub(crate) fn augment(mut deps: Deps) -> Deps {
        if std::env::var("OXA_OWNED_WORKER_USAGE").as_deref() != Ok("1") {
            return deps;
        }
        let emit: Emit = Arc::new(|value| {
            let mut line = serde_json::to_vec(&value)?;
            line.push(b'\n');
            let mut out = io::stdout().lock();
            out.write_all(&line)?;
            out.flush()
        });
        let response_tokens = match std::env::var("OXA_OWNED_WORKER_RESPONSE_TOKENS") {
            Ok(value) if !value.is_empty() => value.parse::<u32>().ok().filter(|n| *n != 0).map(Some),
            Ok(_) | Err(std::env::VarError::NotPresent) => Some(None),
            Err(_) => None,
        };
        let token_threshold = match std::env::var("OXA_OWNED_WORKER_TOKEN_THRESHOLD") {
            Ok(value) if !value.is_empty() => value.parse::<u64>().ok().filter(|n| *n != 0).map(Some),
            Ok(_) | Err(std::env::VarError::NotPresent) => Some(None),
            Err(_) => None,
        };
        let ready = token_threshold.is_some() && response_tokens.is_some() && emit(json!({"type":"oxa_owned_inference","phase":"ready","version":1,"response_tokens":response_tokens.flatten()})).is_ok();
        deps.engine = Arc::new(Meter {
            inner: deps.engine,
            emit,
            counter: Mutex::new(Counter::default()),
            ready,
            model: deps.model.clone(),
            response_tokens: response_tokens.flatten(),
            token_threshold: token_threshold.flatten(),
        });
        deps
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use oxa_fake::{Fault, ScriptTurn, ScriptedEngine};
        use oxa_proto::{FrozenPrefix, Hints, TurnKind, WireMsg};
        #[test]
        fn engine_source_accounts_for_prefill_summary_chat_and_unknown_failure() {
            let mut failed = ScriptTurn::text("unused");
            failed.fault = Some(Fault::Refuse);
            let engine = Arc::new(ScriptedEngine::new(vec![
                ScriptTurn::text("summary"),
                ScriptTurn::text("reply"),
                failed,
            ]));
            let records = Arc::new(Mutex::new(vec![
                json!({"type":"oxa_owned_inference","phase":"ready","version":1}),
            ]));
            let output = records.clone();
            let meter = Meter {
                inner: engine.clone(),
                emit: Arc::new(move |value| {
                    output.lock().unwrap().push(value);
                    Ok(())
                }),
                counter: Mutex::new(Counter::default()),
                ready: true,
                model: Some("fake-model".into()),
                response_tokens: None,
                token_threshold: None,
            };
            let prefix = FrozenPrefix {
                sections: vec![("append".into(), "private-source-marker".into())],
                tools_json: "[]".into(),
                sha256: [0; 32],
            };
            let messages = vec![WireMsg::user("private-task-marker")];
            let cancel = CancelToken::new();
            let mut expected = 0;
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
                        session: "owned",
                        idem_key: "",
                        kind,
                        background: kind != TurnKind::Chat,
                        snapshot_keep: false,
                    },
                };
                assert_eq!(
                    meter.count_prompt(&request).unwrap(),
                    engine.count_prompt(&request).unwrap()
                );
                let turn = meter.chat(&request, &cancel, &mut |_| {}).unwrap();
                expected += u64::from(turn.usage.prompt) + u64::from(turn.usage.completion);
            }
            let mut ledger = super::super::Ledger::default();
            for record in records.lock().unwrap().iter() {
                ledger.consume(record.clone()).unwrap();
            }
            assert_eq!(ledger.total, expected);
            assert!(ledger.settled());
            meter.clear_hint("owned").unwrap();
            assert_eq!(engine.clears(), vec!["owned"]);
            let request = ChatRequest {
                prefix: &prefix,
                messages: &messages,
                model: "fake-model",
                max_tokens: 128,
                thinking: false,
                preserve_thinking: false,
                sampling: None,
                hints: Hints {
                    session: "owned",
                    idem_key: "",
                    kind: TurnKind::Compaction,
                    background: true,
                    snapshot_keep: false,
                },
            };
            assert!(meter.chat(&request, &cancel, &mut |_| {}).is_err());
            for record in records.lock().unwrap().iter().skip(7) {
                ledger.consume(record.clone()).unwrap();
            }
            assert!(ledger.unknown);
            assert!(!ledger.settled());
            assert_eq!(ledger.total, expected);
            let wire = serde_json::to_string(&*records.lock().unwrap()).unwrap();
            assert!(!wire.contains("private-source-marker") && !wire.contains("private-task-marker"));
        }
        #[test]
        fn observed_threshold_refuses_the_next_sdk_call_without_parent_polling() {
            for exact in [false, true] {
                let mut turn = ScriptTurn::text("overrun");
                turn.completion_tokens = Some(1);
                let inner = Arc::new(ScriptedEngine::new(vec![turn; 2]));
                let prefix = FrozenPrefix {
                    sections: vec![],
                    tools_json: "[]".into(),
                    sha256: [0; 32],
                };
                let messages = vec![WireMsg::user("observed threshold")];
                let mut request = ChatRequest {
                    prefix: &prefix,
                    messages: &messages,
                    model: "fake-model",
                    max_tokens: 128,
                    thinking: false,
                    preserve_thinking: false,
                    sampling: None,
                    hints: Hints {
                        session: "owned",
                        idem_key: "",
                        kind: TurnKind::Chat,
                        background: false,
                        snapshot_keep: false,
                    },
                };
                let threshold = if exact {
                    inner.count_prompt(&request).unwrap().unwrap() + 1
                } else {
                    1
                };
                let records = Arc::new(Mutex::new(vec![
                    json!({"type":"oxa_owned_inference","phase":"ready","version":1}),
                ]));
                let output = records.clone();
                let meter = Meter {
                    inner: inner.clone(),
                    emit: Arc::new(move |record| {
                        output.lock().unwrap().push(record);
                        Ok(())
                    }),
                    counter: Mutex::new(Counter::default()),
                    ready: true,
                    model: Some("fake-model".into()),
                    response_tokens: None,
                    token_threshold: Some(threshold),
                };
                let reply = meter.chat(&request, &CancelToken::new(), &mut |_| {}).unwrap();
                let observed = u64::from(reply.usage.prompt) + u64::from(reply.usage.completion);
                assert!(observed >= threshold);
                if exact {
                    assert_eq!(observed, threshold);
                }
                for kind in [TurnKind::Chat, TurnKind::Compaction, TurnKind::Prefill] {
                    request.hints.kind = kind;
                    request.max_tokens = if kind == TurnKind::Prefill { 0 } else { 128 };
                    assert_eq!(
                        meter
                            .chat(&request, &CancelToken::new(), &mut |_| {})
                            .unwrap_err()
                            .code,
                        "token_budget"
                    );
                }
                assert_eq!(
                    inner.requests().len(),
                    1,
                    "no SDK dispatch after observation, without parent polling"
                );
                let mut ledger = super::super::Ledger::default();
                for record in records.lock().unwrap().iter() {
                    ledger.consume(record.clone()).unwrap();
                }
                assert_eq!(ledger.total, observed, "observed usage is never clamped");
                assert_eq!(ledger.refused_threshold, Some(threshold));
                assert!(
                    ledger.settled(),
                    "pre-dispatch refusals do not invent unknown usage"
                );
            }
        }
        #[test]
        fn response_allowance_clamps_each_kind_without_expanding_smaller_requests() {
            let inner = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok"); 4]));
            let meter = Meter {
                inner: inner.clone(),
                emit: Arc::new(|_| Ok(())),
                counter: Mutex::new(Counter::default()),
                ready: true,
                model: Some("fake-model".into()),
                response_tokens: Some(512),
                token_threshold: None,
            };
            let prefix = FrozenPrefix {
                sections: vec![],
                tools_json: "[]".into(),
                sha256: [0; 32],
            };
            assert_eq!(meter.info(None).unwrap().max_output, 512);
            let messages = vec![WireMsg::user("bounded response")];
            for (kind, requested, expected) in [
                (TurnKind::Chat, 4096, 512),
                (TurnKind::Compaction, 2048, 512),
                (TurnKind::Chat, 128, 128),
                (TurnKind::Prefill, 0, 0),
            ] {
                let request = ChatRequest {
                    prefix: &prefix,
                    messages: &messages,
                    model: "fake-model",
                    max_tokens: requested,
                    thinking: false,
                    preserve_thinking: false,
                    sampling: None,
                    hints: Hints {
                        session: "owned",
                        idem_key: "",
                        kind,
                        background: kind != TurnKind::Chat,
                        snapshot_keep: false,
                    },
                };
                meter.chat(&request, &CancelToken::new(), &mut |_| {}).unwrap();
                assert_eq!(inner.requests().last().unwrap().max_tokens, expected);
            }
        }
        #[test]
        fn contained_stream_panic_drains_accounting_with_unknown_usage() {
            let inner = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("partial reply")]));
            let records = Arc::new(Mutex::new(vec![
                json!({"type":"oxa_owned_inference","phase":"ready","version":1}),
            ]));
            let output = records.clone();
            let meter = Meter {
                inner,
                emit: Arc::new(move |record| {
                    output.lock().unwrap().push(record);
                    Ok(())
                }),
                counter: Mutex::new(Counter::default()),
                ready: true,
                model: Some("fake-model".into()),
                response_tokens: None,
                token_threshold: None,
            };
            let prefix = FrozenPrefix {
                sections: vec![],
                tools_json: "[]".into(),
                sha256: [0; 32],
            };
            let messages = vec![WireMsg::user("synthetic panic request")];
            let request = ChatRequest {
                prefix: &prefix,
                messages: &messages,
                model: "fake-model",
                max_tokens: 128,
                thinking: false,
                preserve_thinking: false,
                sampling: None,
                hints: Hints {
                    session: "owned",
                    idem_key: "",
                    kind: TurnKind::Chat,
                    background: false,
                    snapshot_keep: false,
                },
            };
            assert!(oxa_proto::panic::contain(|| meter.chat(
                &request,
                &CancelToken::new(),
                &mut |_| panic!("synthetic stream callback failure")
            ))
            .is_err());
            let mut ledger = super::super::Ledger::default();
            for record in records.lock().unwrap().iter() {
                ledger.consume(record.clone()).unwrap();
            }
            assert!(
                ledger.drained(),
                "contained panic must not leave an active inference forever"
            );
            assert!(ledger.unknown && !ledger.settled());
            assert_eq!(ledger.total, 0, "unknown does not fabricate a charge or refund");
            assert_eq!(records.lock().unwrap().len(), 3, "one ready/start/end, no retry");
        }
        #[test]
        fn unavailable_accounting_refuses_before_inference() {
            let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("must not run")]));
            let meter = Meter {
                inner: engine.clone(),
                emit: Arc::new(|_| Err(io::Error::other("closed sink"))),
                counter: Mutex::new(Counter::default()),
                ready: true,
                model: Some("fake-model".into()),
                response_tokens: None,
                token_threshold: None,
            };
            let prefix = FrozenPrefix {
                sections: vec![],
                tools_json: "[]".into(),
                sha256: [0; 32],
            };
            let messages = vec![WireMsg::user("task")];
            let request = ChatRequest {
                prefix: &prefix,
                messages: &messages,
                model: "fake-model",
                max_tokens: 128,
                thinking: false,
                preserve_thinking: false,
                sampling: None,
                hints: Hints {
                    session: "owned",
                    idem_key: "",
                    kind: TurnKind::Chat,
                    background: false,
                    snapshot_keep: false,
                },
            };
            assert!(meter.chat(&request, &CancelToken::new(), &mut |_| {}).is_err());
            assert!(engine.requests().is_empty());
            let fixed = Meter {
                inner: engine.clone(),
                emit: Arc::new(|_| Ok(())),
                counter: Mutex::new(Counter::default()),
                ready: true,
                model: Some("different-model".into()),
                response_tokens: None,
                token_threshold: None,
            };
            assert_eq!(
                fixed
                    .chat(&request, &CancelToken::new(), &mut |_| {})
                    .unwrap_err()
                    .code,
                "policy_violation"
            );
            assert!(engine.requests().is_empty());
        }
    }
}
#[cfg(feature = "interactive-control")]
pub(crate) use native::augment;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn record(ledger: &mut Ledger, value: Value) -> io::Result<()> {
        let mut value = value;
        value["type"] = json!("oxa_owned_inference");
        ledger.consume(value)
    }
    #[test]
    fn known_requests_include_prefill_and_summary_without_double_charging() {
        let mut ledger = Ledger::default();
        record(&mut ledger, json!({"phase":"ready","version":1})).unwrap();
        for (id, tokens) in [(1, 12), (2, 9), (3, 7)] {
            record(&mut ledger, json!({"phase":"start","id":id})).unwrap();
            assert!(!ledger.settled());
            record(&mut ledger, json!({"phase":"end","id":id,"tokens":tokens})).unwrap();
        }
        assert_eq!(ledger.total, 28);
        assert!(ledger.settled());
        assert!(ledger.seen);
        assert!(record(&mut ledger, json!({"phase":"end","id":3,"tokens":7})).is_err());
        assert_eq!(ledger.total, 28);
    }
    #[test]
    fn active_bound_and_overflow_fail_closed() {
        let mut ledger = Ledger::default();
        record(&mut ledger, json!({"phase":"ready","version":1})).unwrap();
        for id in 1..=8 {
            record(&mut ledger, json!({"phase":"start","id":id})).unwrap();
        }
        assert!(record(&mut ledger, json!({"phase":"start","id":9})).is_err());
        assert_eq!(ledger.active.len(), 8);
        assert!(ledger.unknown);
        let mut ledger = Ledger::default();
        record(&mut ledger, json!({"phase":"ready","version":1})).unwrap();
        for (id, tokens) in [(1, u64::MAX), (2, 1)] {
            record(&mut ledger, json!({"phase":"start","id":id})).unwrap();
            let result = record(&mut ledger, json!({"phase":"end","id":id,"tokens":tokens}));
            if id == 1 {
                result.unwrap();
            } else {
                assert!(result.is_err());
            }
        }
        assert_eq!(ledger.total, u64::MAX);
        assert!(ledger.unknown);
        assert!(!ledger.settled());
    }
    #[test]
    fn failed_or_unfinished_calls_never_become_known_zero() {
        let mut ledger = Ledger::default();
        record(&mut ledger, json!({"phase":"ready","version":1})).unwrap();
        record(&mut ledger, json!({"phase":"start","id":1})).unwrap();
        assert!(!ledger.settled());
        record(&mut ledger, json!({"phase":"end","id":1,"tokens":null})).unwrap();
        assert!(ledger.unknown);
        assert!(!ledger.settled());
        assert!(record(&mut ledger, json!({"phase":"ready","version":1})).is_err());
        assert!(record(&mut ledger, json!({"phase":"start","id":1})).is_err());
    }
}
