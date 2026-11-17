//! Session-owner publication extracted from #129 f89ba2e control.rs::Publisher.
//! Callers supply approved summaries, not raw/private transcripts. This module
//! authenticates nobody, performs no I/O, and never owns a session or task.
use super::{History, Page, HISTORY_BYTES};
use crate::workspace::bounded_json;
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct State {
    latest: Option<Value>,
    history: History,
    session: Option<Value>,
    tasks: Value,
}

/// Cloneable publication handle for one existing session owner.
#[derive(Clone, Default)]
pub struct Publisher(Arc<Mutex<State>>);

#[derive(Serialize)]
struct SessionSummary<'a> {
    id: &'a str,
    model: &'a str,
    mode: &'a str,
    busy: bool,
}

#[derive(Serialize)]
struct StatusSummary<'a> {
    goal: &'a Value,
    kind: &'a str,
    session: Option<&'a Value>,
    tasks: &'a Value,
}

#[derive(Serialize)]
struct EventSummary<'a> {
    kind: &'a str,
    goal: &'a Value,
    reply: Option<ReplyPreview<'a>>,
    request_id: Option<&'a str>,
    session: Option<&'a Value>,
}

#[derive(Serialize)]
struct ReplyPreview<'a> {
    text: &'a str,
    truncated: bool,
}

fn bounded(value: &impl Serialize) -> Result<(), &'static str> {
    bounded_json(value, HISTORY_BYTES).map_err(|_| "follow summary exceeds byte limit")
}

fn encode_summary(value: &impl Serialize) -> Result<Value, &'static str> {
    bounded(value)?;
    serde_json::to_value(value).map_err(|_| "invalid follow summary")
}

impl Publisher {
    /// Bind approved metadata for subsequent publications. Earlier observations
    /// retain their original session identity; this does not switch a runtime.
    pub fn bind_session(&self, id: &str, model: &str, mode: &str, busy: bool) -> Result<(), &'static str> {
        let session = encode_summary(&SessionSummary {
            id,
            model,
            mode,
            busy,
        })?;
        self.0.lock().unwrap_or_else(|e| e.into_inner()).session = Some(session);
        Ok(())
    }

    /// Supply an approved task summary, never task state or execution authority.
    /// It is included in the next status publication, not rewritten into history.
    pub fn tasks(&self, tasks: Value) -> Result<(), &'static str> {
        bounded(&tasks)?;
        self.0.lock().unwrap_or_else(|e| e.into_inner()).tasks = tasks;
        Ok(())
    }

    /// Publish the owner's observation. Status refreshes coalesce into `latest`
    /// without consuming event cursors. Replies have an explicit 16-KiB UTF-8
    /// preview. Rejection leaves both latest observation and history unchanged.
    pub fn publish(
        &self,
        kind: &str,
        goal: Value,
        reply: Option<&str>,
        request_id: Option<&str>,
    ) -> Result<Option<u64>, &'static str> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let latest = encode_summary(&StatusSummary {
            goal: &goal,
            kind,
            session: state.session.as_ref(),
            tasks: &state.tasks,
        })?;
        if kind == "goal_status" {
            state.latest = Some(latest);
            return Ok(None);
        }
        let reply = reply.map(|text| {
            let mut end = text.len().min(16 * 1024);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            ReplyPreview {
                text: &text[..end],
                truncated: end != text.len(),
            }
        });
        let event = encode_summary(&EventSummary {
            kind,
            goal: &goal,
            reply,
            request_id,
            session: state.session.as_ref(),
        })
        .map_err(|_| "event exceeds follow history byte limit")?;
        let sequence = state.history.publish(event)?;
        state.latest = Some(latest);
        Ok(Some(sequence))
    }

    /// Last published observation, not a fresh read of another session's state.
    pub fn latest(&self) -> Option<Value> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).latest.clone()
    }

    pub fn page(&self, after: u64) -> Result<Page, &'static str> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .history
            .page(after)
    }
}
