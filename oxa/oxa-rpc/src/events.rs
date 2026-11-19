//! C9: JSON records for events, messages and entries.
// ported from pi packages/coding-agent/docs/json.md and src/modes/json (record shapes)
use oxa_core::State;
use oxa_proto::{
    codes, AgentEvent, CompactionResult, Entry, EntryBody, EventMessage, EventSink, MessageDelta, OxaError,
    Role, StopReason, ToolCall, Usage, Value,
};
use oxa_session::{encode_header, encode_visible_entry as encode_entry, SessionHeader};
use serde_json::{json, Map};
use std::io::Write;

/// The JSON record of an event: `{"type": e.name(), ..fields}` with pi's camelCase field names
/// and shapes field for field (json.md). `message_update` is delta-only.
pub fn event_to_json(e: &AgentEvent) -> Value {
    let mut m = record(e.name());
    match e {
        AgentEvent::AgentStart | AgentEvent::TurnStart | AgentEvent::SummarizationRetryFinished => {}
        AgentEvent::AgentEnd { messages, will_retry } => {
            put(
                &mut m,
                "messages",
                Value::Array(messages.iter().map(message_to_json).collect()),
            );
            put(&mut m, "willRetry", *will_retry);
        }
        AgentEvent::AgentSettled { aborted } => put(&mut m, "aborted", *aborted),
        AgentEvent::TurnEnd {
            message,
            tool_results,
        } => {
            put(&mut m, "message", message_to_json(message));
            put(
                &mut m,
                "toolResults",
                Value::Array(tool_results.iter().map(message_to_json).collect()),
            );
        }
        AgentEvent::MessageStart { message } | AgentEvent::MessageEnd { message } => {
            put(&mut m, "message", message_to_json(message));
        }
        AgentEvent::MessageUpdate { usage, delta } => {
            put(&mut m, "usage", usage_json(*usage));
            put(&mut m, "assistantMessageEvent", delta_json(delta));
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            put(&mut m, "toolCallId", tool_call_id.as_str());
            put(&mut m, "toolName", tool_name.as_str());
            put(&mut m, "args", args.clone());
        }
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial,
        } => {
            put(&mut m, "toolCallId", tool_call_id.as_str());
            put(&mut m, "toolName", tool_name.as_str());
            put(&mut m, "args", args.clone());
            put(&mut m, "partialResult", text_result(partial, json!({})));
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            duration_ms,
        } => {
            let details = if result.ui.is_null() {
                json!({})
            } else {
                result.ui.clone()
            };
            put(&mut m, "toolCallId", tool_call_id.as_str());
            put(&mut m, "toolName", tool_name.as_str());
            put(&mut m, "result", text_result(&result.text, details));
            put(&mut m, "isError", result.is_error);
            if let Some(ms) = duration_ms {
                put(&mut m, "durationMs", *ms);
            }
        }
        AgentEvent::CompactionStart { reason } => put(&mut m, "reason", reason.as_str()),
        AgentEvent::CompactionEnd {
            reason,
            result,
            aborted,
            will_retry,
            error_message,
        } => {
            put(&mut m, "reason", reason.as_str());
            if let Some(r) = result {
                put(&mut m, "result", compaction_result_json(r));
            }
            put(&mut m, "aborted", *aborted);
            put(&mut m, "willRetry", *will_retry);
            put_opt(&mut m, "errorMessage", error_message.as_deref());
        }
        AgentEvent::AutoRetryStart {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        }
        | AgentEvent::SummarizationRetryScheduled {
            attempt,
            max_attempts,
            delay_ms,
            error_message,
        } => {
            put(&mut m, "attempt", *attempt);
            put(&mut m, "maxAttempts", *max_attempts);
            put(&mut m, "delayMs", *delay_ms);
            put(&mut m, "errorMessage", error_message.as_str());
        }
        AgentEvent::AutoRetryEnd {
            success,
            attempt,
            final_error,
        } => {
            put(&mut m, "success", *success);
            put(&mut m, "attempt", *attempt);
            put_opt(&mut m, "finalError", final_error.as_deref());
        }
        AgentEvent::SummarizationRetryAttemptStart { source, reason } => {
            put(&mut m, "source", *source);
            if let Some(r) = reason {
                put(&mut m, "reason", r.as_str());
            }
        }
        AgentEvent::QueueUpdate { steering, follow_up } => {
            put(&mut m, "steering", str_array(steering));
            put(&mut m, "followUp", str_array(follow_up));
        }
        AgentEvent::EntryAppended { entry } => put(&mut m, "entry", entry_to_json(entry)),
        AgentEvent::ModelSelect { model, previous } => {
            put(&mut m, "model", model.as_str());
            put_opt(&mut m, "previousModel", previous.as_deref());
        }
        AgentEvent::ThinkingLevelChanged { level } => put(&mut m, "level", level.as_str()),
        AgentEvent::SessionInfoChanged { name } => put_opt(&mut m, "name", name.as_deref()),
        AgentEvent::BashExecutionUpdate { id, delta } => {
            put_opt(&mut m, "id", id.as_deref());
            put(&mut m, "delta", delta.as_str());
        }
        AgentEvent::ExtensionError {
            extension_path,
            event,
            error,
        } => {
            put(&mut m, "extensionPath", extension_path.as_str());
            put(&mut m, "event", event.as_str());
            put(&mut m, "error", error.as_str());
        }
        // ---- oxa's own events (additive) ----
        AgentEvent::Prefill { done, total, secs } => {
            put(&mut m, "done", *done);
            put(&mut m, "total", *total);
            put(&mut m, "secs", *secs);
        }
        AgentEvent::EngineQueue { pos, wait_s } => {
            if let Some(p) = pos {
                put(&mut m, "pos", *p);
            }
            put(&mut m, "waitS", *wait_s);
        }
        AgentEvent::PrefixEpoch {
            sha256,
            reason,
            reprefill_tokens,
        } => {
            put(&mut m, "sha256", hex(sha256));
            put(&mut m, "reason", reason.as_str());
            put(&mut m, "reprefillTokens", *reprefill_tokens);
        }
        AgentEvent::CacheMiss {
            expected,
            cached,
            cause,
        } => {
            put(&mut m, "expected", *expected);
            put(&mut m, "cached", *cached);
            put(&mut m, "cause", *cause);
        }
        AgentEvent::ModeChanged { mode } => put(&mut m, "mode", mode.as_str()),
        AgentEvent::Notice { level, text } => {
            put(&mut m, "level", level.as_str());
            put(&mut m, "text", text.as_str());
        }
        AgentEvent::ToolRepair {
            attempt,
            cause,
            total,
            stopped,
        } => {
            put(&mut m, "attempt", *attempt);
            put(&mut m, "cause", *cause);
            put(&mut m, "total", *total);
            put(&mut m, "stopped", *stopped);
        }
    }
    Value::Object(m)
}

/// pi's message object: user `{role, content, timestamp}`; assistant `{role, content: [blocks:
/// thinking, text, toolCall], stopReason, usage{input, output, cacheRead, cacheWrite,
/// totalTokens, cost{..zeros}}, model, provider: "oxe", api: "openai-completions", timestamp[,
/// errorMessage]}`; tool result `{role: "toolResult", toolCallId, toolName, content: [text],
/// isError, timestamp}`. `usage.input` = prompt - cached, `cacheRead` = cached.
pub fn message_to_json(m: &EventMessage) -> Value {
    let content = m.msg.content.clone().unwrap_or_default();
    match m.msg.role {
        Role::Assistant => {
            let mut blocks = Vec::new();
            if let Some(r) = m.msg.reasoning.as_deref().filter(|r| !r.is_empty()) {
                blocks.push(json!({"type": "thinking", "thinking": r}));
            }
            if !content.is_empty() {
                blocks.push(json!({"type": "text", "text": content}));
            }
            blocks.extend(m.msg.tool_calls.iter().map(tool_call_json));
            let mut o = Map::new();
            put(&mut o, "role", "assistant");
            put(&mut o, "content", Value::Array(blocks));
            put(
                &mut o,
                "stopReason",
                m.stop.unwrap_or(StopReason::Pending).as_str(),
            );
            put(&mut o, "usage", usage_json(m.usage.unwrap_or_default()));
            put(&mut o, "model", m.model.clone().unwrap_or_default());
            put(&mut o, "provider", "oxe");
            put(&mut o, "api", "openai-completions");
            put(&mut o, "timestamp", m.ts_ms);
            put_opt(&mut o, "errorMessage", m.error_message.as_deref());
            Value::Object(o)
        }
        Role::Tool => json!({
            "role": "toolResult",
            "toolCallId": m.msg.tool_call_id.clone().unwrap_or_default(),
            "toolName": m.tool_name.clone().unwrap_or_default(),
            "content": [{"type": "text", "text": content}],
            "isError": m.is_error,
            "timestamp": m.ts_ms,
        }),
        Role::User | Role::System => json!({
            "role": m.msg.role.as_str(),
            "content": content,
            "timestamp": m.ts_ms,
        }),
    }
}

/// The visible entry JSON; session-private provider replay is excluded.
pub fn entry_to_json(e: &Entry) -> Value {
    let mut buf = Vec::new();
    encode_entry(e, &mut buf);
    serde_json::from_slice(&buf).unwrap_or(Value::Null)
}

/// The first JSON-mode record: `{"type":"session","version":3,"id":..,"timestamp":..,"cwd":..}`.
pub fn session_header_record(h: &SessionHeader) -> Value {
    let mut buf = Vec::new();
    encode_header(h, &mut buf);
    serde_json::from_slice(&buf).unwrap_or(Value::Null)
}

pub(crate) fn session_state(session: &oxa_core::Session) -> Value {
    let state = state_to_json(&session.state());
    #[cfg(target_os = "linux")]
    let state = {
        let mut state = state;
        state["goalRevision"] = Value::from(session.goal_revision());
        state
    };
    state
}

/// The RPC `get_state` data object (pi's field names; oxa adds `mode`, `prefixSha256`).
pub fn state_to_json(s: &State) -> Value {
    let mut o = Map::new();
    put(
        &mut o,
        "model",
        json!({"id": s.model, "name": s.model, "provider": "oxe", "api": "openai-completions"}),
    );
    put(&mut o, "thinkingLevel", s.thinking.as_str());
    put(&mut o, "isStreaming", s.is_streaming);
    put(&mut o, "isCompacting", s.is_compacting);
    put(&mut o, "steeringMode", queue_mode_str(s.steering_mode));
    put(&mut o, "followUpMode", queue_mode_str(s.follow_up_mode));
    put(
        &mut o,
        "sessionFile",
        match &s.session_file {
            Some(p) => Value::from(p.to_string_lossy().into_owned()),
            None => Value::Null,
        },
    );
    put(&mut o, "sessionId", s.session_id.as_str());
    put(
        &mut o,
        "statusFile",
        s.status_file.as_ref().map(|p| p.to_string_lossy().into_owned()),
    );
    put_opt(&mut o, "sessionName", s.session_name.as_deref());
    put(&mut o, "autoCompactionEnabled", s.auto_compaction);
    put(&mut o, "autoRetryEnabled", s.auto_retry);
    put(&mut o, "messageCount", s.message_count);
    put(&mut o, "pendingMessageCount", s.pending_message_count);
    put(&mut o, "mode", s.mode.as_str());
    put(&mut o, "prefixSha256", hex(&s.prefix_sha256));
    Value::Object(o)
}

/// Write one record: compact JSON + `\n` (LF only), flushed. One `write_all` per record.
pub fn write_record(out: &mut dyn Write, v: &Value) -> Result<(), OxaError> {
    let mut line = serde_json::to_vec(v)
        .map_err(|e| OxaError::new(codes::INTERNAL, format!("cannot encode a record: {e}")))?;
    line.push(b'\n');
    out.write_all(&line)
        .map_err(|e| OxaError::io("cannot write a record", &e))?;
    out.flush().map_err(|e| OxaError::io("cannot flush records", &e))
}

/// An `EventSink` that writes each event as a record.
pub struct JsonSink<W: Write> {
    pub out: W,
    /// First write error, if any (the sink keeps accepting events).
    pub error: Option<OxaError>,
}

/// Whether an event becomes a JSON / RPC record. pi documents `entry_appended` only for an
/// entry an extension appended (`appendEntry`, a `custom` entry, json.md); the core also
/// reports every other append in-process (front ends draw stored messages from them), but
/// those stay off the wire: messages arrive as `message_*`, compactions as `compaction_end`.
pub(crate) fn on_wire(e: &AgentEvent) -> bool {
    match e {
        AgentEvent::EntryAppended { entry } => matches!(entry.body, EntryBody::Custom { .. }),
        _ => true,
    }
}

impl<W: Write> EventSink for JsonSink<W> {
    fn emit(&mut self, e: &AgentEvent) {
        if !on_wire(e) {
            return;
        }
        let record = event_to_json(e);
        if let Err(err) = write_record(&mut self.out, &record) {
            self.error.get_or_insert(err);
        }
    }
}

// ---- helpers ----

fn record(ty: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("type".to_string(), Value::from(ty));
    m
}

fn put(m: &mut Map<String, Value>, key: &str, v: impl Into<Value>) {
    m.insert(key.to_string(), v.into());
}

fn put_opt(m: &mut Map<String, Value>, key: &str, v: Option<&str>) {
    if let Some(v) = v {
        put(m, key, v);
    }
}

fn str_array(items: &[String]) -> Value {
    Value::Array(items.iter().map(|s| Value::from(s.as_str())).collect())
}

/// pi's `usage`: `input = prompt - cached`, `cacheRead = cached`, `cacheWrite` is always 0 on
/// our engine, costs are zero (no pricing).
fn usage_json(u: Usage) -> Value {
    let input = u.prompt.saturating_sub(u.cached);
    let output = u.completion;
    let cache_read = u.cached;
    let total = input as u64 + output as u64 + cache_read as u64;
    json!({
        "input": input,
        "output": output,
        "cacheRead": cache_read,
        "cacheWrite": 0,
        "totalTokens": total,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0},
    })
}

/// pi's tool-result content: one text block plus `details`.
fn text_result(text: &str, details: Value) -> Value {
    json!({"content": [{"type": "text", "text": text}], "details": details})
}

/// pi's `toolCall` block. `arguments` is the parsed object; text that is not JSON is kept as a
/// string so nothing is lost in the record.
fn tool_call_json(t: &ToolCall) -> Value {
    let arguments =
        serde_json::from_str::<Value>(&t.args_raw).unwrap_or_else(|_| Value::from(t.args_raw.as_str()));
    json!({"type": "toolCall", "id": t.id, "name": t.name, "arguments": arguments})
}

fn delta_json(d: &MessageDelta) -> Value {
    match d {
        MessageDelta::TextStart { index } => json!({"type": "text_start", "contentIndex": index}),
        MessageDelta::TextDelta { index, delta } => {
            json!({"type": "text_delta", "contentIndex": index, "delta": delta})
        }
        MessageDelta::TextEnd { index, content } => {
            json!({"type": "text_end", "contentIndex": index, "content": content})
        }
        MessageDelta::ThinkingStart { index } => json!({"type": "thinking_start", "contentIndex": index}),
        MessageDelta::ThinkingDelta { index, delta } => {
            json!({"type": "thinking_delta", "contentIndex": index, "delta": delta})
        }
        MessageDelta::ThinkingEnd { index, content } => {
            json!({"type": "thinking_end", "contentIndex": index, "content": content})
        }
        MessageDelta::ToolcallStart { index, id, name } => {
            json!({"type": "toolcall_start", "contentIndex": index, "id": id, "toolName": name})
        }
        MessageDelta::ToolcallDelta { index, delta } => {
            json!({"type": "toolcall_delta", "contentIndex": index, "delta": delta})
        }
        MessageDelta::ToolcallEnd { index, call } => {
            json!({"type": "toolcall_end", "contentIndex": index, "toolCall": tool_call_json(call)})
        }
    }
}

fn compaction_result_json(r: &CompactionResult) -> Value {
    json!({
        "summary": r.summary,
        "firstKeptEntryId": r.first_kept_entry_id,
        "tokensBefore": r.tokens_before,
        "estimatedTokensAfter": r.estimated_tokens_after,
        "usage": r.usage.map_or(Value::Null, usage_json),
        "details": r.details,
    })
}

fn queue_mode_str(m: oxa_config::QueueMode) -> &'static str {
    match m {
        oxa_config::QueueMode::All => "all",
        oxa_config::QueueMode::OneAtATime => "one-at-a-time",
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
