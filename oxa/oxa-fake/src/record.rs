//! A14: request records.
use crate::Recorded;
use oxa_proto::{codes, OxaError, Role, ToolCall, Value, WireMsg};
use serde_json::json;
use std::io::Write;
use std::path::Path;

fn invalid(msg: impl Into<String>) -> OxaError {
    OxaError::new(codes::INVALID, msg)
}

/// Build a [`Recorded`] from a received request: lowercased `headers` and the raw JSON `body`
/// of `/v1/chat/completions`. `messages[0]` with role `system` becomes `system`; `tools` is
/// kept as the raw substring of the body (not re-serialised); the remaining messages are
/// decoded into `WireMsg` (`arguments` -> `args_raw` verbatim). Hints come from the headers
/// (`x-oxe-session` or `x-session-id`, `idempotency-key`, `x-oxe-turn-kind`,
/// `x-oxe-priority`, `x-oxe-snapshot`). `reply` and `usage` are filled in by the server.
pub fn parse_recorded(seq: usize, headers: &[(String, String)], body: &[u8]) -> Result<Recorded, OxaError> {
    let text = std::str::from_utf8(body).map_err(|_| invalid("request body is not UTF-8"))?;
    let v: Value =
        serde_json::from_str(text).map_err(|e| invalid(format!("request body is not JSON: {e}")))?;
    let obj = v
        .as_object()
        .ok_or_else(|| invalid("request body is not a JSON object"))?;

    let model = obj.get("model").and_then(Value::as_str).unwrap_or("").to_string();
    let max_tokens = obj
        .get("max_tokens")
        .and_then(Value::as_u64)
        .and_then(|x| u32::try_from(x).ok())
        .ok_or_else(|| invalid("`max_tokens` must be a non-negative integer"))?;
    let thinking = obj
        .get("chat_template_kwargs")
        .and_then(|k| k.get("enable_thinking"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let preserve_thinking = obj
        .get("chat_template_kwargs")
        .and_then(|k| k.get("preserve_thinking"))
        .and_then(Value::as_bool);
    let temperature = obj.get("temperature").and_then(Value::as_f64).unwrap_or(0.0);

    let msgs = obj
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("`messages` must be an array"))?;
    let mut system = String::new();
    let mut messages = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        let w = wire_from_value(m)?;
        if i == 0 && w.role == Role::System {
            system = w.content.unwrap_or_default();
        } else {
            messages.push(w);
        }
    }

    let tools_json = raw_top_member(text, "tools").unwrap_or("[]").to_string();

    let h = |name: &str| -> Option<String> {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let session = h("x-oxe-session")
        .or_else(|| h("x-session-id"))
        .unwrap_or_default();
    let idem_key = h("idempotency-key").unwrap_or_default();
    let kind = h("x-oxe-turn-kind").unwrap_or_default();
    let background = h("x-oxe-priority").as_deref() == Some("background");
    let snapshot_keep = h("x-oxe-snapshot").as_deref() == Some("keep");

    Ok(Recorded {
        seq,
        system,
        tools_json,
        messages,
        model,
        max_tokens,
        thinking,
        session,
        idem_key,
        kind,
        background,
        snapshot_keep,
        preserve_thinking,
        temperature,
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect(),
        body: body.to_vec(),
        reply: None,
        usage: None,
    })
}

/// Decode one wire message of a request (canonical shape of `WireMsg::write_json`).
fn wire_from_value(m: &Value) -> Result<WireMsg, OxaError> {
    let role_s = m
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("message without a role"))?;
    let role = Role::parse(role_s).ok_or_else(|| invalid(format!("unknown role `{role_s}`")))?;
    let content = match m.get("content") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(invalid("message content must be a string or null")),
    };
    let mut out = WireMsg {
        role,
        content,
        reasoning: None,
        tool_calls: Vec::new(),
        tool_call_id: None,
    };
    match role {
        Role::Tool => {
            out.tool_call_id = m.get("tool_call_id").and_then(Value::as_str).map(str::to_string);
        }
        Role::Assistant => {
            out.reasoning = m
                .get("reasoning_content")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                for c in calls {
                    let f = c
                        .get("function")
                        .ok_or_else(|| invalid("tool call without a function"))?;
                    let id = c.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                    let name = f.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let args_raw = match f.get("arguments") {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => String::new(),
                    };
                    out.tool_calls.push(ToolCall { id, name, args_raw });
                }
            }
        }
        Role::System | Role::User => {}
    }
    Ok(out)
}

/// The raw text of the top-level member `key` of the JSON object `text`, exactly as written
/// (no re-serialisation). `None` when absent or when `text` is not an object.
fn raw_top_member<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let b = text.as_bytes();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    loop {
        i = skip_ws(b, i);
        if b.get(i) == Some(&b'}') {
            return None;
        }
        let key_end = string_end(b, i)?;
        let name = &text[i + 1..key_end - 1];
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            return None;
        }
        i = skip_ws(b, i + 1);
        let value_start = i;
        let value_stop = value_end(b, i)?;
        if name == key {
            return Some(text[value_start..value_stop].trim_end());
        }
        i = value_stop;
        if b.get(i) == Some(&b',') {
            i += 1;
        } else {
            return None;
        }
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    i
}

/// Index just after the closing quote of the string starting at `i`.
fn string_end(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// Index of the `,` or closing `}`/`]` that ends the value starting at `i` (depth 0).
fn value_end(b: &[u8], i: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = i;
    while j < b.len() {
        match b[j] {
            b'"' => {
                j = string_end(b, j)?;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                if depth == 0 {
                    return Some(j);
                }
                depth -= 1;
            }
            b',' if depth == 0 => return Some(j),
            _ => {}
        }
        j += 1;
    }
    None
}

/// Append records to a JSON Lines log (one object per request: seq, headers, body as a
/// string, reply, usage) for the golden runner and for debugging.
pub fn write_log(path: &Path, records: &[Recorded]) -> Result<(), OxaError> {
    let mut out = Vec::new();
    for r in records {
        let headers: Vec<Value> = r.headers.iter().map(|(k, v)| json!([k, v])).collect();
        let reply = match &r.reply {
            None => Value::Null,
            Some(m) => json!({
                "role": m.role.as_str(),
                "content": m.content,
                "reasoning": m.reasoning,
                "tool_calls": m.tool_calls.iter()
                    .map(|t| json!({"id": t.id, "name": t.name, "args_raw": t.args_raw}))
                    .collect::<Vec<_>>(),
            }),
        };
        let usage = match r.usage {
            None => Value::Null,
            Some(u) => json!({"prompt": u.prompt, "completion": u.completion, "cached": u.cached}),
        };
        let line = json!({
            "seq": r.seq,
            "headers": headers,
            "body": String::from_utf8_lossy(&r.body),
            "reply": reply,
            "usage": usage,
        });
        out.extend_from_slice(&serde_json::to_vec(&line).expect("record serialises"));
        out.push(b'\n');
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| OxaError::io("open record file", &e))?;
    f.write_all(&out)
        .map_err(|e| OxaError::io("write record file", &e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_member_keeps_bytes() {
        let t = "{\"model\":\"m\",\"tools\":[{\"b\":1,\"a\":\"x,]}\"}],\"stream\":true}";
        assert_eq!(raw_top_member(t, "tools"), Some("[{\"b\":1,\"a\":\"x,]}\"}]"));
        assert_eq!(raw_top_member(t, "absent"), None);
    }
}
