//! Invocation-local, text-free terminal evidence. Nothing is restored from session history.
use oxa_core::{Progress, ProgressObserver, RunOutcome, Session};
use oxa_proto::OxaError;
use oxa_proto::{TurnKind, Usage};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct State {
    opened: bool,
    goal: Option<Value>,
    finished: bool,
    session_id: Option<String>,
    requests: Option<u64>,
    turns: Option<u64>,
    active: u64,
    balanced: bool,
    prompt: Option<u64>,
    completion: Option<u64>,
    cached: Option<u64>,
    unreported: Option<u64>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            opened: false,
            goal: None,
            finished: false,
            session_id: None,
            requests: Some(0),
            turns: Some(0),
            active: 0,
            balanced: true,
            prompt: Some(0),
            completion: Some(0),
            cached: Some(0),
            unreported: Some(0),
        }
    }
}
fn add(total: &mut Option<u64>, value: u64) {
    *total = total.and_then(|total| total.checked_add(value));
}

#[derive(Debug, Default)]
pub(crate) struct ResultFacts {
    state: Mutex<State>,
    status: Option<Arc<crate::status::StatusWriter>>,
    billing: Option<oxc_gateway::diagnostics::Diagnostics>,
}
impl ResultFacts {
    pub(crate) fn with_billing(mut self, billing: oxc_gateway::diagnostics::Diagnostics) -> Self {
        self.billing = Some(billing);
        self
    }

    pub(crate) fn new(status: Option<Arc<crate::status::StatusWriter>>) -> Self {
        Self {
            status,
            ..Self::default()
        }
    }
    pub(crate) fn attach(self: &Arc<Self>, session: &mut Session) -> Result<(), OxaError> {
        session.set_progress_observer(self.clone());
        self.status.as_ref().map_or(Ok(()), |status| status.check())
    }
    pub(crate) fn goal(&self, goal: Option<oxa_session::Goal>) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).goal = goal.map(|g| {
            serde_json::json!({
                "status":g.status.as_str(), "evidence":g.evidence, "turns":g.turns,
                "tokens_used":g.tokens_used, "elapsed_ms":g.elapsed_ms,
            "unknown_requests":g.unknown_requests,
            })
        });
    }
    pub(crate) fn session(&self, saved_id: Option<&str>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.finished {
            return;
        }
        state.opened = true;
        state.session_id = saved_id.map(str::to_owned);
    }
    pub(crate) fn request_start(&self, kind: TurnKind) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.finished {
            return;
        }
        add(&mut state.requests, 1);
        if matches!(kind, TurnKind::Chat | TurnKind::Subagent) {
            add(&mut state.turns, 1);
        }
        if let Some(active) = state.active.checked_add(1) {
            state.active = active;
        } else {
            state.balanced = false;
        }
    }
    pub(crate) fn request_end(&self, usage: Option<Usage>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.finished {
            return;
        }
        if state.active == 0 {
            state.balanced = false;
        } else {
            state.active -= 1;
        }
        if let Some(usage) = usage {
            add(&mut state.prompt, usage.prompt.into());
            add(&mut state.completion, usage.completion.into());
            if usage.cached_missing {
                state.cached = None;
            } else {
                add(&mut state.cached, usage.cached.into());
            }
        } else {
            add(&mut state.unreported, 1);
        }
    }

    /// Freeze the evidence at the terminal record so a late helper cannot revise that claim.
    pub(crate) fn record(outcome: RunOutcome, facts: Option<&Self>) -> Value {
        let mut record = outcome.record();
        let mut guard = facts.map(|facts| facts.state.lock().unwrap_or_else(|p| p.into_inner()));
        if let Some(state) = &mut guard {
            state.finished = true;
        }
        let state = guard.as_ref().filter(|state| state.opened);
        record["session_id"] = state
            .as_ref()
            .map_or(Value::Null, |state| json!(state.session_id));
        record["requests"] = state.as_ref().map_or(Value::Null, |state| json!(state.requests));
        record["turns"] = state.as_ref().map_or(Value::Null, |state| json!(state.turns));
        record["tokens"] = state.as_ref().map_or(Value::Null, |state| {
            json!({
                "prompt":state.prompt, "completion":state.completion, "cached":state.cached,
                "complete":state.balanced && state.active == 0 && state.unreported == Some(0)
                    && state.prompt.is_some() && state.completion.is_some() && state.cached.is_some(),
                "unreported_requests":state.unreported,
            })
        });
        if let Some(billing) = facts
            .and_then(|f| f.billing.as_ref())
            .and_then(|b| b.last_billing())
        {
            record["last_billing"] = billing.record();
        }
        if let Some(goal) = state.as_ref().and_then(|state| state.goal.as_ref()) {
            record["goal"] = goal.clone();
        }
        record["usage_scope"] = json!("process");
        // Successful tool calls and generated text are not checked workspace/commit evidence.
        record["files_changed"] = Value::Null;
        record["files_changed_evidence"] = json!("unavailable");
        record
    }
}

impl ProgressObserver for ResultFacts {
    fn notify(&self, session: &oxa_session::SessionRef, progress: Progress<'_>) {
        match progress {
            Progress::Session { file } => self.session(file.map(|_| session.id.as_str())),
            Progress::RequestStart { kind, .. } => self.request_start(kind),
            Progress::RequestEnd { usage, .. } => self.request_end(usage),
            _ => {}
        }
        if let Some(status) = &self.status {
            status.notify(session, progress);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn usage(prompt: u32, completion: u32, cached: u32) -> Usage {
        Usage {
            prompt,
            completion,
            cached,
            cached_missing: false,
        }
    }

    #[test]
    fn totals_include_summary_prefill_and_discarded_requests_but_only_chat_turns() {
        let facts = ResultFacts::default();
        facts.session(Some("saved-session"));
        for kind in [
            TurnKind::Chat,
            TurnKind::Compaction,
            TurnKind::Prefill,
            TurnKind::Chat,
        ] {
            facts.request_start(kind);
            facts.request_end(Some(usage(100, 4, 60)));
        }
        let record = ResultFacts::record(RunOutcome::Done, Some(&facts));
        assert_eq!(record["session_id"], "saved-session");
        assert_eq!(record["requests"], 4);
        assert_eq!(record["turns"], 2);
        assert_eq!(
            record["tokens"],
            json!({"prompt":400,"completion":16,"cached":240,"complete":true,"unreported_requests":0})
        );
        assert!(record["files_changed"].is_null());
        assert_eq!(record["files_changed_evidence"], "unavailable");
    }

    #[test]
    fn absent_usage_and_cache_counts_stay_explicitly_unknown_after_later_success() {
        let facts = ResultFacts::default();
        facts.session(None);
        facts.request_start(TurnKind::Chat);
        facts.request_end(None);
        facts.request_start(TurnKind::Chat);
        facts.request_end(Some(Usage {
            cached_missing: true,
            ..usage(100, 4, 0)
        }));
        facts.request_start(TurnKind::Prefill);
        facts.request_end(Some(usage(100, 0, 60)));
        let record = ResultFacts::record(RunOutcome::Done, Some(&facts));
        assert!(record["session_id"].is_null());
        assert_eq!(record["requests"], 3);
        assert_eq!(record["turns"], 2);
        assert_eq!(
            record["tokens"],
            json!({"prompt":200,"completion":4,"cached":null,"complete":false,"unreported_requests":1})
        );
    }

    #[test]
    fn session_switches_keep_process_totals_and_late_helpers_cannot_change_the_record() {
        let facts = ResultFacts::default();
        facts.session(Some("first"));
        facts.request_start(TurnKind::Chat);
        facts.request_end(Some(usage(10, 1, 0)));
        facts.session(Some("second"));
        facts.request_start(TurnKind::Prefill);
        let terminal = ResultFacts::record(RunOutcome::Interrupted, Some(&facts));
        assert_eq!(terminal["session_id"], "second");
        assert_eq!(terminal["tokens"]["complete"], false);
        facts.request_end(Some(usage(10, 0, 10)));
        facts.session(Some("third"));
        assert_eq!(
            ResultFacts::record(RunOutcome::Interrupted, Some(&facts)),
            terminal
        );
    }

    #[test]
    fn preflight_failure_is_unknown_while_an_observed_empty_session_is_zero() {
        let record = ResultFacts::record(RunOutcome::Failed("usage"), None);
        for key in ["session_id", "requests", "turns", "tokens", "files_changed"] {
            assert!(record.as_object().unwrap().contains_key(key));
            assert!(record[key].is_null(), "{key}");
        }
        let facts = ResultFacts::default();
        facts.session(Some("saved-session"));
        let record = ResultFacts::record(RunOutcome::NothingToDo, Some(&facts));
        assert_eq!(record["turns"], 0);
        assert_eq!(record["tokens"]["prompt"], 0);
        assert_eq!(record["tokens"]["complete"], true);
    }
}
