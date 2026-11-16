//! A14: the fake-script format.
use crate::{Fault, ScriptTurn};
use oxa_proto::{codes, Finish, OxaError, ToolCall, Value};
use serde_json::{json, Map};

/// Parse a script (format: `docs/fake-script.md`): JSON Lines, one turn object per line; blank
/// lines and lines starting with `#` are skipped. An unknown key or a bad value is `invalid`
/// with the line number.
pub fn parse_script(text: &str) -> Result<Vec<ScriptTurn>, OxaError> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        out.push(parse_line(line, n)?);
    }
    Ok(out)
}

/// Write turns in the script format; `parse_script(script_to_text(x)) == x`.
pub fn script_to_text(turns: &[ScriptTurn]) -> String {
    let mut out = String::new();
    for t in turns {
        let mut m = Map::new();
        if let Some(text) = &t.text {
            m.insert("text".into(), json!(text));
        }
        if let Some(r) = &t.reasoning {
            m.insert("reasoning".into(), json!(r));
        }
        if !t.tool_calls.is_empty() {
            let calls: Vec<Value> = t
                .tool_calls
                .iter()
                .map(|c| json!({"id": c.id, "name": c.name, "args": c.args_raw}))
                .collect();
            m.insert("tool_calls".into(), Value::Array(calls));
        }
        m.insert("finish".into(), json!(finish_str(t.finish)));
        if let Some(c) = t.completion_tokens {
            m.insert("completion_tokens".into(), json!(c));
        }
        m.insert("chunk".into(), json!(t.chunk));
        if let Some(f) = &t.fault {
            m.insert("fault".into(), fault_to_json(f));
        }
        out.push_str(&serde_json::to_string(&Value::Object(m)).expect("script turn serialises"));
        out.push('\n');
    }
    out
}

fn finish_str(f: Finish) -> &'static str {
    match f {
        Finish::Stop => "stop",
        Finish::ToolCalls => "tool_calls",
        Finish::Length => "length",
        Finish::ContentFilter => "content_filter",
    }
}

fn fault_to_json(f: &Fault) -> Value {
    match f {
        Fault::Http { status, body } => json!({"kind": "http", "status": status, "body": body}),
        Fault::Overflow => json!({"kind": "overflow"}),
        Fault::DropAt { bytes } => json!({"kind": "drop_at", "bytes": bytes}),
        Fault::Refuse => json!({"kind": "refuse"}),
        Fault::HoldHeaders { ms } => json!({"kind": "hold_headers", "ms": ms}),
        Fault::CommentsOnly { ms } => json!({"kind": "comments_only", "ms": ms}),
        Fault::EarlyClose => json!({"kind": "early_close"}),
        Fault::Queued { count, pos } => json!({"kind": "queued", "count": count, "pos": pos}),
        Fault::StreamError {
            queued,
            status,
            error_type,
            event,
        } => {
            json!({"kind": "stream_error", "queued": queued, "status": status, "type": error_type, "event": event})
        }
        Fault::Busy { retry_after } => json!({"kind": "busy", "retry_after": retry_after}),
        Fault::SnapNull { reason } => json!({"kind": "snap_null", "reason": reason}),
        Fault::Stall { bytes, ms } => json!({"kind": "stall", "bytes": bytes, "ms": ms}),
        Fault::Cold => json!({"kind": "cold"}),
    }
}

fn bad(line: usize, msg: impl Into<String>) -> OxaError {
    OxaError::new(codes::INVALID, format!("script line {line}: {}", msg.into()))
}

fn check_keys(obj: &Map<String, Value>, allowed: &[&str], line: usize) -> Result<(), OxaError> {
    for k in obj.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(bad(line, format!("unknown key `{k}`")));
        }
    }
    Ok(())
}

fn opt_str(obj: &Map<String, Value>, key: &str, line: usize) -> Result<Option<String>, OxaError> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| bad(line, format!("`{key}` must be a string"))),
    }
}

fn req_str(obj: &Map<String, Value>, key: &str, line: usize) -> Result<String, OxaError> {
    opt_str(obj, key, line)?.ok_or_else(|| bad(line, format!("missing `{key}`")))
}

fn req_u64(obj: &Map<String, Value>, key: &str, line: usize) -> Result<u64, OxaError> {
    obj.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| bad(line, format!("`{key}` must be a non-negative integer")))
}

fn opt_u32(obj: &Map<String, Value>, key: &str, line: usize) -> Result<Option<u32>, OxaError> {
    match obj.get(key) {
        None => Ok(None),
        Some(_) => u32::try_from(req_u64(obj, key, line)?)
            .map(Some)
            .map_err(|_| bad(line, format!("`{key}` out of range"))),
    }
}

fn opt_bool(obj: &Map<String, Value>, key: &str, line: usize) -> Result<Option<bool>, OxaError> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| bad(line, format!("`{key}` must be true or false"))),
    }
}

/// The OpenAI error `type` the engine and the controller use for a status (`api::error_json`,
/// `openai_api.ERROR_TYPES`).
pub(crate) fn default_error_type(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        409 => "conflict_error",
        429 => "rate_limit_error",
        501 => "not_supported_error",
        503 => "unavailable_error",
        _ => "server_error",
    }
}

fn parse_line(line: &str, n: usize) -> Result<ScriptTurn, OxaError> {
    let v: Value = serde_json::from_str(line).map_err(|e| bad(n, format!("not JSON: {e}")))?;
    let obj = v.as_object().ok_or_else(|| bad(n, "a turn is a JSON object"))?;
    check_keys(
        obj,
        &[
            "text",
            "reasoning",
            "tool_calls",
            "finish",
            "completion_tokens",
            "chunk",
            "fault",
        ],
        n,
    )?;

    let text = opt_str(obj, "text", n)?;
    let reasoning = opt_str(obj, "reasoning", n)?;

    let mut tool_calls = Vec::new();
    if let Some(v) = obj.get("tool_calls") {
        let arr = v
            .as_array()
            .ok_or_else(|| bad(n, "`tool_calls` must be an array"))?;
        for c in arr {
            let co = c
                .as_object()
                .ok_or_else(|| bad(n, "a tool call is a JSON object"))?;
            check_keys(co, &["id", "name", "args"], n)?;
            tool_calls.push(ToolCall {
                id: req_str(co, "id", n)?,
                name: req_str(co, "name", n)?,
                args_raw: req_str(co, "args", n)?,
            });
        }
    }

    let finish = match obj.get("finish") {
        None => {
            if tool_calls.is_empty() {
                Finish::Stop
            } else {
                Finish::ToolCalls
            }
        }
        Some(v) => match v.as_str() {
            Some("stop") => Finish::Stop,
            Some("tool_calls") => Finish::ToolCalls,
            Some("length") => Finish::Length,
            Some("content_filter") => Finish::ContentFilter,
            _ => return Err(bad(n, "`finish` must be \"stop\", \"tool_calls\" or \"length\"")),
        },
    };

    let completion_tokens = match obj.get("completion_tokens") {
        None => None,
        Some(_) => {
            let x = req_u64(obj, "completion_tokens", n)?;
            Some(u32::try_from(x).map_err(|_| bad(n, "`completion_tokens` out of range"))?)
        }
    };

    let chunk = match obj.get("chunk") {
        None => 0,
        Some(_) => {
            let x = req_u64(obj, "chunk", n)?;
            usize::try_from(x).map_err(|_| bad(n, "`chunk` out of range"))?
        }
    };

    let fault = match obj.get("fault") {
        None => None,
        Some(v) => Some(parse_fault(v, n)?),
    };

    Ok(ScriptTurn {
        reasoning,
        text,
        tool_calls,
        finish,
        completion_tokens,
        chunk,
        fault,
    })
}

fn parse_fault(v: &Value, n: usize) -> Result<Fault, OxaError> {
    let obj = v
        .as_object()
        .ok_or_else(|| bad(n, "`fault` must be a JSON object"))?;
    let kind = req_str(obj, "kind", n)?;
    match kind.as_str() {
        "http" => {
            check_keys(obj, &["kind", "status", "body"], n)?;
            let status =
                u16::try_from(req_u64(obj, "status", n)?).map_err(|_| bad(n, "`status` out of range"))?;
            Ok(Fault::Http {
                status,
                body: req_str(obj, "body", n)?,
            })
        }
        "overflow" => {
            check_keys(obj, &["kind"], n)?;
            Ok(Fault::Overflow)
        }
        "drop_at" => {
            check_keys(obj, &["kind", "bytes"], n)?;
            let bytes =
                usize::try_from(req_u64(obj, "bytes", n)?).map_err(|_| bad(n, "`bytes` out of range"))?;
            Ok(Fault::DropAt { bytes })
        }
        "refuse" => {
            check_keys(obj, &["kind"], n)?;
            Ok(Fault::Refuse)
        }
        "hold_headers" => {
            check_keys(obj, &["kind", "ms"], n)?;
            Ok(Fault::HoldHeaders {
                ms: req_u64(obj, "ms", n)?,
            })
        }
        "comments_only" => {
            check_keys(obj, &["kind", "ms"], n)?;
            Ok(Fault::CommentsOnly {
                ms: req_u64(obj, "ms", n)?,
            })
        }
        "early_close" => {
            check_keys(obj, &["kind"], n)?;
            Ok(Fault::EarlyClose)
        }
        "queued" => {
            check_keys(obj, &["kind", "count", "pos"], n)?;
            Ok(Fault::Queued {
                count: opt_u32(obj, "count", n)?.unwrap_or(1),
                pos: opt_bool(obj, "pos", n)?.unwrap_or(true),
            })
        }
        "stream_error" => {
            check_keys(obj, &["kind", "queued", "status", "type", "event"], n)?;
            let status =
                u16::try_from(req_u64(obj, "status", n)?).map_err(|_| bad(n, "`status` out of range"))?;
            Ok(Fault::StreamError {
                queued: opt_u32(obj, "queued", n)?.unwrap_or(1),
                status,
                error_type: match opt_str(obj, "type", n)? {
                    Some(t) => t,
                    None => default_error_type(status).to_string(),
                },
                event: opt_bool(obj, "event", n)?.unwrap_or(true),
            })
        }
        "busy" => {
            check_keys(obj, &["kind", "retry_after"], n)?;
            let retry_after = match obj.get("retry_after") {
                None => Some(30),
                Some(Value::Null) => None,
                Some(_) => opt_u32(obj, "retry_after", n)?,
            };
            Ok(Fault::Busy { retry_after })
        }
        "snap_null" => {
            check_keys(obj, &["kind", "reason"], n)?;
            Ok(Fault::SnapNull {
                reason: opt_str(obj, "reason", n)?
                    .unwrap_or_else(|| "no_boundary_in_last_message".to_string()),
            })
        }
        "stall" => {
            check_keys(obj, &["kind", "bytes", "ms"], n)?;
            let bytes =
                usize::try_from(req_u64(obj, "bytes", n)?).map_err(|_| bad(n, "`bytes` out of range"))?;
            Ok(Fault::Stall {
                bytes,
                ms: req_u64(obj, "ms", n)?,
            })
        }
        "cold" => {
            check_keys(obj, &["kind"], n)?;
            Ok(Fault::Cold)
        }
        other => Err(bad(n, format!("unknown fault kind `{other}`"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_comments() {
        let text = "# a comment\n\n{\"text\":\"hi\"}\n{\"tool_calls\":[{\"id\":\"c1\",\"name\":\"read\",\"args\":\"{}\"}]}\n";
        let turns = parse_script(text).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].finish, Finish::Stop);
        assert_eq!(turns[1].finish, Finish::ToolCalls);
        assert_eq!(turns[0].chunk, 0);
    }

    #[test]
    fn unknown_key_reports_line() {
        let err = parse_script("{\"text\":\"a\"}\n{\"text\":\"b\"}\n{\"nope\":1}\n").unwrap_err();
        assert_eq!(err.code, codes::INVALID);
        assert!(err.message.contains("line 3"), "{}", err.message);
    }

    #[test]
    fn stall_and_cold_round_trip() {
        let text = "{\"text\":\"a\",\"fault\":{\"kind\":\"stall\",\"bytes\":7,\"ms\":900}}\n{\"text\":\"b\",\"fault\":{\"kind\":\"cold\"}}\n";
        let turns = parse_script(text).unwrap();
        assert_eq!(turns[0].fault, Some(Fault::Stall { bytes: 7, ms: 900 }));
        assert_eq!(turns[1].fault, Some(Fault::Cold));
        assert_eq!(parse_script(&script_to_text(&turns)).unwrap(), turns);
        assert!(parse_script("{\"fault\":{\"kind\":\"stall\",\"ms\":1}}\n").is_err());
    }

    #[test]
    fn bad_fault_kind_reports_line() {
        let err = parse_script("{\"fault\":{\"kind\":\"melt\"}}\n").unwrap_err();
        assert!(err.message.contains("line 1"), "{}", err.message);
    }
}
