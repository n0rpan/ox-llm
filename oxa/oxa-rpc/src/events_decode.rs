//! Reading event records back: the inverse of [`event_to_json`](crate::event_to_json).
//!
//! A client of `--mode json` / `--mode rpc` (the desktop app, for a session on another host)
//! turns each record into the same [`AgentEvent`] a local session emits, so one reducer serves
//! both.
//!
//! ## Contract
//! * `event_to_json(&event_from_json(&r)?) == r` for every record `r` that `event_to_json`
//!   writes: decoding and encoding again gives the same record.
//! * The record is narrower than the event, so what the record does not carry is not restored:
//!   images, private provider replay, and `Usage::cached_missing` / `Usage::unreported` come
//!   back empty or `false`; an absent assistant `stopReason` comes back as `Pending`.
//! * `None` for a record that is not an event (a response, an `extension_ui_request`, the
//!   session header), for an unknown `type`, and for a known `type` whose fields do not fit.
//!   Fields the decoder does not know are ignored, so a newer peer may add fields.
use oxa_proto::{
    miss_cause, repair_cause, AgentEvent, CompactReason, CompactionResult, EventMessage, Level, MessageDelta,
    Mode, Role, StopReason, ThinkingLevel, ToolCall, ToolOutput, Usage, Value, WireMsg,
};
use oxa_session::decode_entry;
use serde_json::Map;

type Obj = Map<String, Value>;

/// The event of one JSON / RPC record, or `None` when the record is not an event this build
/// knows (see the module contract).
pub fn event_from_json(v: &Value) -> Option<AgentEvent> {
    let m = v.as_object()?;
    Some(match text(m, "type")? {
        "agent_start" => AgentEvent::AgentStart,
        "agent_end" => AgentEvent::AgentEnd {
            messages: messages(m, "messages")?,
            will_retry: flag(m, "willRetry")?,
        },
        "agent_settled" => AgentEvent::AgentSettled {
            aborted: flag(m, "aborted")?,
        },
        "turn_start" => AgentEvent::TurnStart,
        "turn_end" => AgentEvent::TurnEnd {
            message: message(m.get("message")?)?,
            tool_results: messages(m, "toolResults")?,
        },
        "message_start" => AgentEvent::MessageStart {
            message: message(m.get("message")?)?,
        },
        "message_update" => AgentEvent::MessageUpdate {
            usage: usage(m.get("usage")?)?,
            delta: delta(m.get("assistantMessageEvent")?)?,
        },
        "message_end" => AgentEvent::MessageEnd {
            message: message(m.get("message")?)?,
        },
        "tool_execution_start" => AgentEvent::ToolExecutionStart {
            tool_call_id: owned(m, "toolCallId")?,
            tool_name: owned(m, "toolName")?,
            args: m.get("args")?.clone(),
        },
        "tool_execution_update" => AgentEvent::ToolExecutionUpdate {
            tool_call_id: owned(m, "toolCallId")?,
            tool_name: owned(m, "toolName")?,
            args: m.get("args")?.clone(),
            partial: first_text(m.get("partialResult")?)?,
        },
        "tool_execution_end" => {
            let result = m.get("result")?;
            // The encoder writes `{}` for "no display data".
            let ui = match result.get("details") {
                Some(Value::Object(d)) if d.is_empty() => Value::Null,
                Some(d) => d.clone(),
                None => Value::Null,
            };
            AgentEvent::ToolExecutionEnd {
                tool_call_id: owned(m, "toolCallId")?,
                tool_name: owned(m, "toolName")?,
                result: ToolOutput {
                    text: first_text(result)?,
                    is_error: flag(m, "isError")?,
                    ui,
                    images: Vec::new(),
                },
                duration_ms: opt(m, "durationMs", Value::as_u64)?,
            }
        }
        "compaction_start" => AgentEvent::CompactionStart {
            reason: compact_reason(text(m, "reason")?)?,
        },
        "compaction_end" => AgentEvent::CompactionEnd {
            reason: compact_reason(text(m, "reason")?)?,
            result: opt(m, "result", compaction_result)?,
            aborted: flag(m, "aborted")?,
            will_retry: flag(m, "willRetry")?,
            error_message: opt_owned(m, "errorMessage")?,
        },
        "auto_retry_start" => AgentEvent::AutoRetryStart {
            attempt: num(m, "attempt")?,
            max_attempts: num(m, "maxAttempts")?,
            delay_ms: m.get("delayMs")?.as_u64()?,
            error_message: owned(m, "errorMessage")?,
        },
        "auto_retry_end" => AgentEvent::AutoRetryEnd {
            success: flag(m, "success")?,
            attempt: num(m, "attempt")?,
            final_error: opt_owned(m, "finalError")?,
        },
        "summarization_retry_scheduled" => AgentEvent::SummarizationRetryScheduled {
            attempt: num(m, "attempt")?,
            max_attempts: num(m, "maxAttempts")?,
            delay_ms: m.get("delayMs")?.as_u64()?,
            error_message: owned(m, "errorMessage")?,
        },
        "summarization_retry_attempt_start" => AgentEvent::SummarizationRetryAttemptStart {
            source: intern(&SUMMARY_SOURCES, text(m, "source")?)?,
            reason: opt(m, "reason", |r| compact_reason(r.as_str()?))?,
        },
        "summarization_retry_finished" => AgentEvent::SummarizationRetryFinished,
        "queue_update" => AgentEvent::QueueUpdate {
            steering: strings(m, "steering")?,
            follow_up: strings(m, "followUp")?,
        },
        "entry_appended" => AgentEvent::EntryAppended {
            entry: decode_entry(&serde_json::to_vec(m.get("entry")?).ok()?).ok()?,
        },
        "model_select" => AgentEvent::ModelSelect {
            model: owned(m, "model")?,
            previous: opt_owned(m, "previousModel")?,
        },
        "thinking_level_changed" => AgentEvent::ThinkingLevelChanged {
            level: ThinkingLevel::parse(text(m, "level")?)?,
        },
        "session_info_changed" => AgentEvent::SessionInfoChanged {
            name: opt_owned(m, "name")?,
        },
        "bash_execution_update" => AgentEvent::BashExecutionUpdate {
            id: opt_owned(m, "id")?,
            delta: owned(m, "delta")?,
        },
        "extension_error" => AgentEvent::ExtensionError {
            extension_path: owned(m, "extensionPath")?,
            event: owned(m, "event")?,
            error: owned(m, "error")?,
        },
        // ---- oxa's own events ----
        "prefill" => AgentEvent::Prefill {
            done: num(m, "done")?,
            total: num(m, "total")?,
            secs: num(m, "secs")?,
        },
        "engine_queue" => AgentEvent::EngineQueue {
            pos: opt(m, "pos", as_u32)?,
            wait_s: num(m, "waitS")?,
        },
        "prefix_epoch" => AgentEvent::PrefixEpoch {
            sha256: sha256(text(m, "sha256")?)?,
            reason: owned(m, "reason")?,
            reprefill_tokens: num(m, "reprefillTokens")?,
        },
        "cache_miss" => AgentEvent::CacheMiss {
            expected: num(m, "expected")?,
            cached: num(m, "cached")?,
            // A cause from a newer peer is still a cache miss: `unknown` is the honest reading.
            cause: intern(&MISS_CAUSES, text(m, "cause")?).unwrap_or(miss_cause::UNKNOWN),
        },
        "mode_changed" => AgentEvent::ModeChanged {
            mode: Mode::parse(text(m, "mode")?)?,
        },
        "notice" => AgentEvent::Notice {
            level: level(text(m, "level")?)?,
            text: owned(m, "text")?,
        },
        "tool_repair" => AgentEvent::ToolRepair {
            attempt: num(m, "attempt")?,
            cause: intern(&REPAIR_CAUSES, text(m, "cause")?)?,
            total: num(m, "total")?,
            stopped: flag(m, "stopped")?,
        },
        _ => return None,
    })
}

// ---- messages ----

/// pi's message object back to an [`EventMessage`] (see `message_to_json`).
fn message(v: &Value) -> Option<EventMessage> {
    let m = v.as_object()?;
    let ts_ms = m.get("timestamp")?.as_u64()?;
    let mut out = EventMessage {
        msg: WireMsg::user(""),
        ts_ms,
        stop: None,
        usage: None,
        model: None,
        tool_name: None,
        is_error: false,
        error_message: None,
    };
    match text(m, "role")? {
        "assistant" => {
            let mut msg = WireMsg::assistant("");
            let mut body: Option<String> = None;
            let mut reasoning: Option<String> = None;
            for block in m.get("content")?.as_array()? {
                let b = block.as_object()?;
                match text(b, "type")? {
                    "thinking" => reasoning
                        .get_or_insert_with(String::new)
                        .push_str(text(b, "thinking")?),
                    "text" => body.get_or_insert_with(String::new).push_str(text(b, "text")?),
                    "toolCall" => msg.tool_calls.push(tool_call(block)?),
                    // A block kind this build does not know: keep the rest of the message.
                    _ => {}
                }
            }
            msg.content = body;
            msg.reasoning = reasoning;
            out.msg = msg;
            out.stop = Some(stop_reason(text(m, "stopReason")?)?);
            out.usage = Some(usage(m.get("usage")?)?);
            out.model = opt_owned(m, "model")?.filter(|s| !s.is_empty());
            out.error_message = opt_owned(m, "errorMessage")?;
        }
        "toolResult" => {
            out.msg = WireMsg::tool_result(owned(m, "toolCallId")?, first_text(v)?);
            out.tool_name = Some(owned(m, "toolName")?);
            out.is_error = flag(m, "isError")?;
        }
        role => {
            let role = Role::parse(role).filter(|r| matches!(r, Role::User | Role::System))?;
            let mut msg = WireMsg::user(owned(m, "content")?);
            msg.role = role;
            out.msg = msg;
        }
    }
    Some(out)
}

fn messages(m: &Obj, key: &str) -> Option<Vec<EventMessage>> {
    m.get(key)?.as_array()?.iter().map(message).collect()
}

/// pi's `toolCall` block. The encoder writes `arguments` as the parsed JSON, or as a string when
/// the streamed text was not JSON; the text chosen here encodes to the same `arguments` again.
fn tool_call(v: &Value) -> Option<ToolCall> {
    let m = v.as_object()?;
    let args_raw = match m.get("arguments")? {
        Value::String(s) if serde_json::from_str::<Value>(s).is_err() => s.clone(),
        other => serde_json::to_string(other).ok()?,
    };
    Some(ToolCall {
        id: owned(m, "id")?,
        name: owned(m, "name")?,
        args_raw,
    })
}

/// pi's `usage` back to the engine's counters: `prompt = input + cacheRead`.
fn usage(v: &Value) -> Option<Usage> {
    let m = v.as_object()?;
    let cached = num(m, "cacheRead")?;
    Some(Usage {
        prompt: num(m, "input")?.checked_add(cached)?,
        completion: num(m, "output")?,
        cached,
        ..Usage::default()
    })
}

fn delta(v: &Value) -> Option<MessageDelta> {
    let m = v.as_object()?;
    let index = usize::try_from(m.get("contentIndex")?.as_u64()?).ok()?;
    Some(match text(m, "type")? {
        "text_start" => MessageDelta::TextStart { index },
        "text_delta" => MessageDelta::TextDelta {
            index,
            delta: owned(m, "delta")?,
        },
        "text_end" => MessageDelta::TextEnd {
            index,
            content: owned(m, "content")?,
        },
        "thinking_start" => MessageDelta::ThinkingStart { index },
        "thinking_delta" => MessageDelta::ThinkingDelta {
            index,
            delta: owned(m, "delta")?,
        },
        "thinking_end" => MessageDelta::ThinkingEnd {
            index,
            content: owned(m, "content")?,
        },
        "toolcall_start" => MessageDelta::ToolcallStart {
            index,
            id: owned(m, "id")?,
            name: owned(m, "toolName")?,
        },
        "toolcall_delta" => MessageDelta::ToolcallDelta {
            index,
            delta: owned(m, "delta")?,
        },
        "toolcall_end" => MessageDelta::ToolcallEnd {
            index,
            call: tool_call(m.get("toolCall")?)?,
        },
        _ => return None,
    })
}

fn compaction_result(v: &Value) -> Option<CompactionResult> {
    let m = v.as_object()?;
    Some(CompactionResult {
        summary: owned(m, "summary")?,
        first_kept_entry_id: owned(m, "firstKeptEntryId")?,
        tokens_before: num(m, "tokensBefore")?,
        estimated_tokens_after: num(m, "estimatedTokensAfter")?,
        usage: opt(m, "usage", usage)?,
        details: m.get("details").cloned().unwrap_or(Value::Null),
    })
}

// ---- names ----

const SUMMARY_SOURCES: [&str; 2] = ["compaction", "branchSummary"];

const MISS_CAUSES: [&str; 8] = [
    miss_cause::COMPACTION,
    miss_cause::LEAF_MOVE,
    miss_cause::CONTEXT_EDIT,
    miss_cause::EPOCH,
    miss_cause::EXTENSION,
    miss_cause::COLD_START,
    miss_cause::ENGINE_EVICTED,
    miss_cause::UNKNOWN,
];

const REPAIR_CAUSES: [&str; 7] = [
    repair_cause::UNKNOWN_FUNCTION,
    repair_cause::UNKNOWN_PARAMETER,
    repair_cause::MISSING_REQUIRED,
    repair_cause::UNCLOSED,
    repair_cause::BAD_JSON,
    repair_cause::WRONG_TYPE,
    repair_cause::FORMAT,
];

/// The static name equal to `s` (events carry these names as `&'static str`).
fn intern(names: &[&'static str], s: &str) -> Option<&'static str> {
    names.iter().copied().find(|n| *n == s)
}

fn stop_reason(s: &str) -> Option<StopReason> {
    [
        StopReason::Pending,
        StopReason::Stop,
        StopReason::ToolUse,
        StopReason::Length,
        StopReason::Aborted,
        StopReason::Error,
    ]
    .into_iter()
    .find(|r| r.as_str() == s)
}

fn compact_reason(s: &str) -> Option<CompactReason> {
    [
        CompactReason::Manual,
        CompactReason::Threshold,
        CompactReason::Overflow,
    ]
    .into_iter()
    .find(|r| r.as_str() == s)
}

fn level(s: &str) -> Option<Level> {
    [Level::Info, Level::Warning, Level::Error]
        .into_iter()
        .find(|l| l.as_str() == s)
}

/// 64 lowercase or uppercase hex digits to the 32 bytes of a prefix hash.
fn sha256(s: &str) -> Option<[u8; 32]> {
    let digits = s.as_bytes();
    if digits.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(digits.chunks_exact(2)) {
        let hi = char::from(pair[0]).to_digit(16)?;
        let lo = char::from(pair[1]).to_digit(16)?;
        *byte = u8::try_from(hi * 16 + lo).ok()?;
    }
    Some(out)
}

// ---- fields ----

/// The field when it is there and not `null`.
fn present<'a>(m: &'a Obj, key: &str) -> Option<&'a Value> {
    m.get(key).filter(|v| !v.is_null())
}

fn text<'a>(m: &'a Obj, key: &str) -> Option<&'a str> {
    m.get(key)?.as_str()
}

fn owned(m: &Obj, key: &str) -> Option<String> {
    text(m, key).map(str::to_string)
}

fn flag(m: &Obj, key: &str) -> Option<bool> {
    m.get(key)?.as_bool()
}

fn as_u32(v: &Value) -> Option<u32> {
    u32::try_from(v.as_u64()?).ok()
}

fn num(m: &Obj, key: &str) -> Option<u32> {
    as_u32(m.get(key)?)
}

/// An optional field: `Some(None)` when absent or `null`, `None` when present with a wrong type.
fn opt<T>(m: &Obj, key: &str, read: impl Fn(&Value) -> Option<T>) -> Option<Option<T>> {
    match present(m, key) {
        Some(v) => read(v).map(Some),
        None => Some(None),
    }
}

fn opt_owned(m: &Obj, key: &str) -> Option<Option<String>> {
    opt(m, key, |v| v.as_str().map(str::to_string))
}

fn strings(m: &Obj, key: &str) -> Option<Vec<String>> {
    m.get(key)?
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// The text of pi's `{"content": [{"type": "text", "text": ..}], ..}` (all text blocks joined).
fn first_text(v: &Value) -> Option<String> {
    let mut out = String::new();
    for block in v.get("content")?.as_array()? {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            out.push_str(block.get("text")?.as_str()?);
        }
    }
    Some(out)
}
