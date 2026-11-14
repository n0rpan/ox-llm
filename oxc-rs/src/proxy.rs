//! OpenAI request and reply translations, applied around the private engine hop.
use crate::{daemon::Context, engine, models, paths, request_ledger};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue},
    response::Response,
};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::{io, sync::Arc, time::Instant};

pub(crate) struct BadParam {
    pub(crate) message: String,
    pub(crate) param: Option<String>,
    pub(crate) code: &'static str,
}
impl BadParam {
    pub(crate) fn new(message: impl Into<String>, param: &str, code: &'static str) -> Self {
        Self {
            message: message.into(),
            param: Some(param.to_owned()),
            code,
        }
    }
    fn unsupported(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            param: None,
            code: "invalid_request",
        }
    }
}
#[derive(Default)]
struct Meta {
    warnings: Vec<String>,
    ignored: Vec<String>,
    json_mode: Option<&'static str>,
    schema: Value,
}
impl Meta {
    fn headers(&self, response: &mut Response) {
        for (name, values, separator) in [
            ("x-local-ai-warning", &self.warnings, "; "),
            ("x-local-ai-ignored", &self.ignored, ", "),
        ] {
            if !values.is_empty() {
                if let Ok(value) = HeaderValue::from_str(&values.join(separator)) {
                    response.headers_mut().insert(name, value);
                }
            }
        }
    }
}
fn truth(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}
fn single_choice(
    payload: &mut Map<String, Value>,
    notes: &mut Vec<&'static str>,
) -> Result<(), BadParam> {
    if let Some(n) = payload.remove("n") {
        if n.is_null() || n.as_i64() == Some(1) {
            return Ok(());
        }
        if n.as_i64().is_some_and(|n| n > 1) {
            notes.push("n_refused");
            return Err(BadParam::new("\"n\" > 1 is not supported: this local model server returns one choice per request. Send n=1 (or omit it) and repeat the request for more choices.","n","unsupported_value"));
        }
        return Err(BadParam::new(
            "\"n\" must be a positive integer.",
            "n",
            "invalid_value",
        ));
    }
    Ok(())
}
pub(crate) fn defaults(payload: &mut Map<String, Value>, profile: &Value, chat: bool) {
    for (key, value) in [("temperature", json!(0)), ("cache_prompt", json!(true))] {
        if payload.get(key).is_none_or(Value::is_null) {
            payload.insert(key.into(), value);
        }
    }
    if !["max_tokens", "max_completion_tokens", "n_predict"]
        .iter()
        .any(|k| payload.contains_key(*k))
    {
        let context = profile["context_size"].as_u64().filter(|v| *v > 0);
        let explicit = ["max_output_tokens", "default_max_tokens"]
            .iter()
            .find_map(|key| profile[key].as_u64());
        let limit = match explicit {
            Some(0) => None,
            Some(value) => Some(context.map(|c| c.min(value)).unwrap_or(value)),
            None => Some(context.map(|c| (c / 4).clamp(1, 16384)).unwrap_or(16384)),
        };
        if let Some(limit) = limit {
            payload.insert("max_tokens".into(), json!(limit));
        }
    }
    if chat {
        if let Some(kwargs) = payload
            .entry("chat_template_kwargs")
            .or_insert_with(|| json!({}))
            .as_object_mut()
        {
            kwargs.entry("enable_thinking").or_insert(json!(false));
            kwargs.entry("preserve_thinking").or_insert(json!(true));
        }
    }
}
pub(crate) fn python_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), sorted(&map[key])))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        _ => value.clone(),
    }
}
fn chat_request(
    payload: &mut Map<String, Value>,
    sampling: Option<Vec<String>>,
    notes: &mut Vec<&'static str>,
) -> Result<Meta, BadParam> {
    let mut meta = Meta::default();
    if let Some(effort) = payload.remove("reasoning_effort").filter(|v| !v.is_null()) {
        let effort = effort.as_str().ok_or_else(|| {
            BadParam::new(
                "\"reasoning_effort\" must be a string.",
                "reasoning_effort",
                "invalid_value",
            )
        })?;
        if ["none", "minimal", "low", "medium", "high", "xhigh"].contains(&effort) {
            let mut kwargs = payload
                .remove("chat_template_kwargs")
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            if kwargs.get("enable_thinking").is_none() {
                let enabled = !["none", "minimal"].contains(&effort);
                kwargs["enable_thinking"] = json!(enabled);
                notes.push(if enabled {
                    "reasoning_effort_on"
                } else {
                    "reasoning_effort_off"
                });
            }
            payload.insert("chat_template_kwargs".into(), kwargs);
        } else {
            notes.push("reasoning_effort_unknown");
            meta.warnings.push(format!(
                "reasoning_effort {} is unknown and was ignored",
                python_repr(&effort.chars().take(20).collect::<String>())
            ));
        }
    }
    if let Some(fmt) = payload.remove("response_format").filter(|v| !v.is_null()) {
        let kind = fmt["type"]
            .as_str()
            .filter(|_| fmt.is_object())
            .ok_or_else(|| {
                BadParam::new(
                    "\"response_format\" must be an object with a \"type\".",
                    "response_format",
                    "invalid_value",
                )
            })?;
        let instruction = match kind {
            "text" => {
                payload.insert("response_format".into(), fmt.clone());
                None
            }
            "json_object" => {
                meta.json_mode = Some("object");
                Some("Respond with one valid JSON object only: no code fences, no text before or after it.".to_owned())
            }
            "json_schema" => {
                let spec = &fmt["json_schema"];
                let empty = json!({});
                let schema = spec.get("schema").unwrap_or(&empty);
                if !spec.is_object() || !schema.is_object() {
                    return Err(BadParam::new("\"response_format.json_schema\" must be an object with a \"schema\" object.","response_format.json_schema","invalid_value"));
                }
                if spec["strict"] == true {
                    return Err(BadParam::new("\"response_format\" json_schema with \"strict\": true is not supported: this local model server has no grammar-constrained sampling, so it cannot guarantee the schema. Send \"strict\": false (best effort: the schema is given to the model and the reply is checked) or use tools.","response_format.json_schema.strict","unsupported_value"));
                }
                let encoded = sorted(schema).to_string();
                if encoded.chars().count() > 65536 {
                    return Err(BadParam::new(
                        "\"response_format.json_schema.schema\" is too large.",
                        "response_format.json_schema.schema",
                        "invalid_value",
                    ));
                }
                meta.json_mode = Some("schema");
                meta.schema = spec["schema"].clone();
                let name = spec["name"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .map(|name| format!(" ({name})"))
                    .unwrap_or_default();
                Some(format!("Respond with one valid JSON value only (no code fences, no text before or after it) that matches this JSON Schema{name}: {encoded}"))
            }
            _ => {
                return Err(BadParam::new(
                    format!(
                        "Unknown \"response_format\" type {} (text, json_object or json_schema).",
                        python_repr(&kind.chars().take(40).collect::<String>())
                    ),
                    "response_format.type",
                    "invalid_value",
                ))
            }
        };
        if let Some(instruction) = instruction {
            if let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) {
                let added = if let Some(last) = messages.last_mut().filter(|v| v["role"] == "user")
                {
                    if let Some(content) = last["content"].as_str() {
                        last["content"] = json!(format!("{content}\n\n{instruction}"));
                        true
                    } else if let Some(parts) = last["content"].as_array_mut() {
                        parts.push(json!({"type":"text","text":format!("\n\n{instruction}")}));
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !added {
                    messages.push(json!({"role":"user","content":instruction}));
                }
            }
            notes.push(if kind == "json_object" {
                "response_format_json_object_best_effort"
            } else {
                "response_format_json_schema_best_effort"
            });
            if payload.get("stream") == Some(&json!(true)) {
                meta.warnings
                    .push("response_format is best effort; a streamed reply is not checked".into());
            }
        }
    }
    if let Some(supported) = sampling {
        for key in ["presence_penalty", "frequency_penalty"] {
            if payload
                .get(key)
                .and_then(Value::as_f64)
                .is_some_and(|n| n != 0.0)
                && !supported.iter().any(|v| v == key)
            {
                meta.ignored.push(key.into());
            }
        }
        if let Some(bias) = payload.get("logit_bias") {
            let nonzero = match bias {
                Value::Object(map) => map.values().any(|v| v.as_f64() != Some(0.0)),
                Value::Array(pairs) => pairs
                    .iter()
                    .filter_map(|p| p.as_array().filter(|p| p.len() == 2))
                    .any(|p| p[1].as_f64() != Some(0.0)),
                _ => truth(bias),
            };
            if nonzero && !supported.iter().any(|v| v == "logit_bias") {
                meta.ignored.push("logit_bias".into());
            }
        }
    }
    Ok(meta)
}
fn normalize_chat(payload: &mut Map<String, Value>, notes: &mut Vec<&'static str>) {
    if let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) {
        let mut started = false;
        for message in messages {
            if !message.is_object() {
                continue;
            }
            if [json!("system"), json!("developer")].contains(&message["role"]) {
                if started {
                    let content = if let Some(text) = message["content"].as_str() {
                        text.to_owned()
                    } else {
                        message["content"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|p| p["text"].as_str()).collect())
                            .unwrap_or_default()
                    };
                    *message = json!({"role":"user","content":format!("<system-reminder>\n{content}\n</system-reminder>")});
                    notes.push("late_system_message_to_user");
                }
            } else {
                started = true;
            }
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                let mut changed = false;
                for part in parts {
                    let kind = part["type"].as_str().unwrap_or("");
                    if ["text", "media_marker", "tool_reference"].contains(&kind) {
                        continue;
                    }
                    let kind = if !kind.is_empty()
                        && kind.chars().enumerate().all(|(i, c)| {
                            c == '_'
                                || if i == 0 {
                                    c.is_alphabetic()
                                } else {
                                    c.is_alphanumeric()
                                }
                        }) {
                        kind
                    } else {
                        "attachment"
                    };
                    *part = json!({"type":"text","text":format!("[{kind} omitted: this local model reads text only]")});
                    changed = true;
                }
                if changed {
                    notes.push("non_text_part_replaced");
                }
            }
        }
        if payload.get("tool_choice").is_some_and(Value::is_object) {
            let choice = &payload["tool_choice"];
            let name = if choice["function"].is_object() {
                choice["function"]["name"].as_str()
            } else {
                choice["name"].as_str()
            };
            let selected = payload
                .get("tools")
                .and_then(Value::as_array)
                .map(|tools| {
                    tools
                        .iter()
                        .filter(|tool| name.is_some() && tool["function"]["name"].as_str() == name)
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if selected.is_empty() {
                notes.push("unknown_tool_choice_to_auto");
                payload.insert("tool_choice".into(), json!("auto"));
            } else {
                notes.push("named_tool_choice_to_required");
                payload.insert("tools".into(), json!(selected));
                payload.insert("tool_choice".into(), json!("required"));
            }
        }
    }
}
fn text_len(value: &Value) -> usize {
    value.as_str().map(|s| s.chars().count()).unwrap_or(0)
}
pub(crate) fn complete_usage(value: &mut Value, reasoning: i64) {
    let timings = value["timings"].clone();
    let Some(usage) = value.get_mut("usage").and_then(Value::as_object_mut) else {
        return;
    };
    let mut prompt = usage
        .remove("prompt_tokens_details")
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if prompt["cached_tokens"].as_i64().is_none() {
        prompt["cached_tokens"] =
            json!(timings["cache_n"].as_i64().filter(|n| *n >= 0).unwrap_or(0));
    }
    usage.insert("prompt_tokens_details".into(), prompt);
    let mut completion = usage
        .remove("completion_tokens_details")
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if completion["reasoning_tokens"].as_i64().is_none() {
        completion["reasoning_tokens"] = json!(usage
            .get("completion_tokens")
            .and_then(Value::as_i64)
            .map(|n| n.min(reasoning))
            .unwrap_or(reasoning));
    }
    usage.insert("completion_tokens_details".into(), completion);
}
fn json_value(text: &str) -> Option<(Value, String)> {
    let mut candidates = vec![text.trim()];
    if let Some(block) = text.split("```").nth(1) {
        candidates.push(
            block
                .strip_prefix("json")
                .or_else(|| block.strip_prefix("JSON"))
                .unwrap_or(block)
                .trim(),
        );
    }
    for (opener, closer) in [('{', '}'), ('[', ']')] {
        if let (Some(start), Some(end)) = (text.find(opener), text.rfind(closer)) {
            if start < end {
                candidates.push(&text[start..=end]);
            }
        }
    }
    candidates.into_iter().find_map(|candidate| {
        serde_json::from_str(candidate)
            .ok()
            .map(|value| (value, candidate.to_owned()))
    })
}
fn chat_response(value: &mut Value, meta: &mut Meta) {
    let mut reasoning = 0;
    if let Some(message) = value
        .get_mut("choices")
        .and_then(Value::as_array_mut)
        .and_then(|a| {
            a.iter_mut()
                .find_map(|c| c.get_mut("message").filter(|m| m.is_object()))
        })
    {
        let reasoning_chars = text_len(&message["reasoning_content"]);
        let other = text_len(&message["content"])
            + message["tool_calls"]
                .as_array()
                .map(|calls| {
                    calls
                        .iter()
                        .map(|c| {
                            text_len(&c["function"]["arguments"]) + text_len(&c["function"]["name"])
                        })
                        .sum::<usize>()
                })
                .unwrap_or(0);
        if reasoning_chars > 0 {
            reasoning = reasoning_chars as i64;
        }
        // Store the ratio for after releasing the mutable message borrow.
        let share = if reasoning_chars > 0 {
            reasoning_chars as f64 / (reasoning_chars + other) as f64
        } else {
            0.0
        };
        if meta.json_mode.is_some() && !truth(&message["tool_calls"]) {
            if let Some((found, text)) = message["content"].as_str().and_then(json_value) {
                message["content"] = json!(text);
                if meta.json_mode == Some("object") && !found.is_object() {
                    meta.warnings.push("response_format: the reply is not a JSON object (best effort, not enforced)".into());
                }
                if meta.json_mode == Some("schema") {
                    let kind = meta.schema["type"].as_str().unwrap_or("");
                    let matches = match kind {
                        "object" => found.is_object(),
                        "array" => found.is_array(),
                        "string" => found.is_string(),
                        "boolean" => found.is_boolean(),
                        _ => true,
                    };
                    let problem = if !matches {
                        Some(format!("the reply is not a JSON {kind}"))
                    } else if found.is_object() {
                        meta.schema["required"]
                            .as_array()
                            .map(|required| {
                                required
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .filter(|key| found.get(*key).is_none())
                                    .take(5)
                                    .collect::<Vec<_>>()
                            })
                            .filter(|missing| !missing.is_empty())
                            .map(|missing| {
                                format!("the reply misses required key(s) {}", missing.join(", "))
                            })
                    } else {
                        None
                    };
                    if let Some(problem) = problem {
                        meta.warnings.push(format!(
                            "response_format: {problem} (best effort, not enforced)"
                        ));
                    }
                }
            } else {
                meta.warnings.push(
                    "response_format: the reply is not valid JSON (best effort, not enforced)"
                        .into(),
                );
            }
        }
        reasoning = if reasoning > 0 {
            value["usage"]["completion_tokens"]
                .as_i64()
                .filter(|n| *n > 0)
                .map(|n| ((n as f64 * share).round_ties_even() as i64).min(n))
                .unwrap_or(0)
        } else {
            0
        };
    }
    if value["choices"].is_array() {
        complete_usage(value, reasoning);
    }
}
struct Completion {
    id: String,
    created: i64,
    model: Value,
    echo: String,
    include_usage: bool,
    top_n: Option<usize>,
    first: bool,
}
impl Completion {
    fn request(payload: &mut Map<String, Value>) -> Result<Self, BadParam> {
        let mut prompt = payload.get("prompt").cloned().unwrap_or(json!(""));
        if let Some(prompts) = prompt
            .as_array()
            .filter(|p| !p.is_empty() && p.iter().all(|p| p.is_string() || p.is_array()))
        {
            if prompts.len() != 1 {
                return Err(BadParam::unsupported(
                    "One prompt per request: send a string, or a list with one prompt.",
                ));
            }
            prompt = prompts[0].clone();
        }
        if !prompt.is_string() && !prompt.is_array() {
            return Err(BadParam::unsupported(
                "\"prompt\" must be a string or a list.",
            ));
        }
        if payload
            .get("suffix")
            .is_some_and(|v| !v.is_null() && v != "")
        {
            return Err(BadParam::unsupported(
                "\"suffix\" (insert mode) is not supported by this model server.",
            ));
        }
        let mut random = [0_u8; 12];
        getrandom::fill(&mut random)
            .map_err(|_| BadParam::unsupported("Could not allocate a response identifier."))?;
        let meta = Self {
            id: format!(
                "cmpl-{}",
                random
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ),
            created: chrono::Utc::now().timestamp(),
            model: payload.get("model").cloned().unwrap_or(Value::Null),
            echo: if payload.get("echo") == Some(&json!(true)) {
                prompt.as_str().unwrap_or("").to_owned()
            } else {
                String::new()
            },
            include_usage: payload
                .get("stream_options")
                .is_some_and(|v| v["include_usage"] == true),
            top_n: payload
                .get("logprobs")
                .and_then(Value::as_u64)
                .map(|v| v.min(20) as usize),
            first: true,
        };
        let length = ["max_tokens", "max_completion_tokens"]
            .iter()
            .find_map(|key| payload.get(*key).filter(|v| v.as_i64().is_some()).cloned());
        for key in [
            "prompt",
            "max_tokens",
            "max_completion_tokens",
            "logprobs",
            "echo",
            "suffix",
            "n",
            "best_of",
            "stream_options",
            "user",
        ] {
            payload.remove(key);
        }
        payload.insert("prompt".into(), prompt);
        if let Some(length) = length {
            payload.entry("n_predict").or_insert(length);
        }
        if let Some(top_n) = meta.top_n {
            payload.insert("n_probs".into(), json!(top_n.max(1)));
        }
        Ok(meta)
    }
    fn chunk(&self, text: &str, finish: Value, logprobs: Value) -> Value {
        json!({"id":self.id,"object":"text_completion","created":self.created,"model":self.model,"choices":[{"text":text,"index":0,"logprobs":logprobs,"finish_reason":finish}]})
    }
    fn finish(native: &Value) -> Value {
        json!(
            if [json!("eos"), json!("word")].contains(&native["stop_type"]) {
                "stop"
            } else {
                "length"
            }
        )
    }
    fn usage(native: &Value) -> Value {
        let prompt = native["tokens_evaluated"].as_i64().unwrap_or(0);
        let done = native["tokens_predicted"].as_i64().unwrap_or(0);
        let mut usage =
            json!({"prompt_tokens":prompt,"completion_tokens":done,"total_tokens":prompt+done});
        if let Some(cached) = native["timings"].get("cache_n") {
            usage["prompt_tokens_details"] = json!({"cached_tokens":cached});
        }
        usage
    }
    fn logprobs(&self, native: &Value, mut offset: usize) -> Value {
        let Some(top_n) = self.top_n else {
            return Value::Null;
        };
        let Some(rows) = native["completion_probabilities"].as_array() else {
            return Value::Null;
        };
        let mut tokens = Vec::new();
        let mut probs = Vec::new();
        let mut tops = Vec::new();
        let mut offsets = Vec::new();
        for row in rows.iter().filter(|v| v.is_object()) {
            let token = row["token"].as_str().unwrap_or("");
            tokens.push(json!(token));
            probs.push(row["logprob"].clone());
            offsets.push(json!(offset));
            offset += token.chars().count();
            let mut candidates = Map::new();
            for top in row["top_logprobs"]
                .as_array()
                .into_iter()
                .flatten()
                .take(top_n)
                .filter(|v| v.is_object())
            {
                let name = top["token"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| top.get("token").map(Value::to_string).unwrap_or_default());
                candidates.insert(name, top["logprob"].clone());
            }
            tops.push(Value::Object(candidates));
        }
        json!({"tokens":tokens,"token_logprobs":probs,"top_logprobs":tops,"text_offset":offsets})
    }
    fn response(&self, native: Value) -> Value {
        if native.get("content").is_none() {
            return native;
        }
        let mut value = self.chunk(
            &format!("{}{}", self.echo, native["content"].as_str().unwrap_or("")),
            Self::finish(&native),
            self.logprobs(&native, self.echo.chars().count()),
        );
        value["model"] = native.get("model").unwrap_or(&self.model).clone();
        value["usage"] = Self::usage(&native);
        if native["timings"].is_object() {
            value["timings"] = native["timings"].clone();
        }
        value
    }
    fn event(&mut self, raw: &[u8]) -> Vec<u8> {
        let Some(data) = raw.strip_prefix(b"data:") else {
            return [raw, b"\n\n"].concat();
        };
        if data.trim_ascii() == b"[DONE]" {
            return Vec::new();
        }
        let Ok(native) = serde_json::from_slice::<Value>(data) else {
            return [raw, b"\n\n"].concat();
        };
        if native.get("content").is_none() {
            return [raw, b"\n\n"].concat();
        }
        let mut text = native["content"].as_str().unwrap_or("").to_owned();
        if self.first {
            text = self.echo.clone() + &text;
            self.first = false;
        }
        if native["stop"] != true {
            return frame(&self.chunk(&text, Value::Null, self.logprobs(&native, 0)));
        }
        let mut last = self.chunk(&text, Self::finish(&native), Value::Null);
        let mut output = Vec::new();
        if self.include_usage {
            output.extend(frame(&last));
            last = self.chunk("", Value::Null, Value::Null);
            last["choices"] = json!([]);
            last["usage"] = Self::usage(&native);
        }
        if native["timings"].is_object() {
            last["timings"] = native["timings"].clone();
        }
        output.extend(frame(&last));
        output.extend(b"data: [DONE]\n\n");
        output
    }
}
fn frame(value: &Value) -> Vec<u8> {
    format!("data: {}\n\n", paths::python_json(value)).into_bytes()
}
struct Observation {
    manager: Arc<engine::Manager>,
    key: String,
    profile: Value,
    cache: Value,
    prompt: Value,
}
impl Observation {
    fn capture(&mut self, value: &Value) {
        for (name, destination) in [("cache_n", &mut self.cache), ("prompt_n", &mut self.prompt)] {
            if let Some(value) = value["timings"].get(name).filter(|v| v.as_u64().is_some()) {
                *destination = value.clone();
            }
        }
    }
    fn complete(self) {
        self.manager
            .harness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .complete(&self.key, &self.profile, self.cache, self.prompt);
    }
}
// Own the record and slot together: cancellation on any await/body drop still
// records the outcome, and a blocked advisory write never blocks the executor.
struct RequestRecord {
    entry: Option<request_ledger::Entry>,
    admission: Option<engine::Admission>,
    status: u16,
    error: Option<&'static str>,
}
impl RequestRecord {
    fn observe(&mut self, bytes: &[u8]) {
        if let Some(entry) = self.entry.as_mut() {
            entry.observe(bytes);
        }
    }
    fn complete(&mut self) {
        if self.error == Some("client_disconnected") {
            self.error = None;
        }
    }
    fn fail(&mut self, status: u16, error: &'static str) {
        self.status = status;
        self.error = Some(error);
    }
}
impl Drop for RequestRecord {
    fn drop(&mut self) {
        let mut admission = self.admission.take();
        if let Some(admission) = admission.as_mut() {
            admission.begin_finalizing();
        }
        let entry = self.entry.take();
        let (status, error) = (self.status, self.error);
        tokio::task::spawn_blocking(move || {
            if let Some(entry) = entry {
                entry.finish(status, error);
            }
            drop(admission);
        });
    }
}
fn finalize(
    mut response: Response,
    record: RequestRecord,
    observation: Option<Observation>,
) -> Response {
    let body = std::mem::replace(response.body_mut(), Body::empty());
    let stream = futures_util::stream::unfold(
        (body.into_data_stream(), record, observation),
        |(mut body, mut record, observation)| async move {
            match body.next().await {
                Some(chunk) => Some((chunk, (body, record, observation))),
                None => {
                    if let Some(observation) = observation {
                        observation.complete();
                    }
                    record.complete();
                    drop(record);
                    None
                }
            }
        },
    );
    *response.body_mut() = Body::from_stream(stream);
    response
}
struct StreamShim {
    buffer: Vec<u8>,
    reasoning: i64,
    completion: Option<Completion>,
    responses: Option<crate::responses::Stream>,
    observation: Option<Observation>,
}
impl StreamShim {
    fn event(&mut self, raw: &[u8]) -> Vec<u8> {
        if let Some(observation) = self.observation.as_mut() {
            if let Some(data) = raw.strip_prefix(b"data:") {
                if let Ok(value) = serde_json::from_slice::<Value>(data) {
                    observation.capture(&value);
                }
            }
        }
        if let Some(responses) = self.responses.as_mut() {
            return responses.event(raw);
        }
        if let Some(completion) = self.completion.as_mut() {
            return completion.event(raw);
        }
        if let Some(data) = raw.strip_prefix(b"data:") {
            if let Ok(mut value) = serde_json::from_slice::<Value>(data) {
                self.reasoning += value["choices"]
                    .as_array()
                    .map(|choices| {
                        choices
                            .iter()
                            .filter(|c| {
                                c["delta"]["reasoning_content"]
                                    .as_str()
                                    .is_some_and(|s| !s.is_empty())
                            })
                            .count() as i64
                    })
                    .unwrap_or(0);
                if value["usage"].is_object() {
                    complete_usage(&mut value, self.reasoning);
                    return frame(&value);
                }
            }
        }
        [raw, b"\n\n"].concat()
    }
    fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.buffer.extend_from_slice(bytes);
        let mut output = Vec::new();
        while let Some(end) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            let raw = self.buffer.drain(..end + 2).collect::<Vec<_>>();
            output.extend(self.event(&raw[..end]));
        }
        output
    }
    fn finish(&mut self) -> Vec<u8> {
        let raw = std::mem::take(&mut self.buffer);
        if let Some(responses) = self.responses.as_mut() {
            let mut output = if raw.trim_ascii().is_empty() {
                Vec::new()
            } else {
                responses.event(&raw)
            };
            output.extend(responses.finish());
            return output;
        }
        if self.completion.is_some() && !raw.trim_ascii().is_empty() {
            self.event(&raw)
        } else {
            raw
        }
    }
}
fn normalize_error(status: u16, mut value: Value) -> Value {
    let Some(error) = value.get_mut("error") else {
        return value;
    };
    if !error.is_object() {
        *error = json!({"message":error.as_str().map(str::to_owned).unwrap_or_else(||error.to_string())});
    }
    let message = error["message"].as_str().unwrap_or("").to_owned();
    let lower = message.to_lowercase();
    let kind = error["type"].as_str().unwrap_or("").to_owned();
    let code = error["code"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            {
                if ["exceed_context_size_error", "context_length_exceeded"].contains(&kind.as_str())
                    || [
                        "context size",
                        "context length",
                        "exceeds the available context",
                    ]
                    .iter()
                    .any(|s| lower.contains(s))
                {
                    "context_length_exceeded"
                } else if status == 404 && lower.contains("model") {
                    "model_not_found"
                } else {
                    match status {
                        400 => "invalid_request",
                        401 => "invalid_api_key",
                        403 => "forbidden",
                        404 => "not_found",
                        411 => "length_required",
                        413 => "request_too_large",
                        429 => "rate_limit_exceeded",
                        503 => "service_unavailable",
                        _ if status >= 500 => "server_error",
                        _ => "invalid_request",
                    }
                }
            }
            .into()
        });
    let fallback = match code.as_str() {
        "context_length_exceeded" => json!("messages"),
        "model_not_found" => json!("model"),
        _ => Value::Null,
    };
    let param = error["param"]
        .as_str()
        .map(|v| json!(v))
        .unwrap_or(fallback);
    error["message"] = json!(message);
    error["type"] = json!(if kind.is_empty() {
        match status {
            400 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            404 => "invalid_request_error",
            429 => "rate_limit_error",
            _ => "server_error",
        }
    } else {
        &kind
    });
    error["code"] = json!(code);
    error["param"] = param;
    value
}

pub async fn handle(
    context: &Context,
    manager: Arc<engine::Manager>,
    config: &Value,
    path: &str,
    mut payload: Value,
    headers: HeaderMap,
) -> Response {
    let profile = match models::resolve(config, payload["model"].as_str()) {
        Ok(profile) => profile,
        Err(message) => {
            return context.error(
                404,
                message,
                "invalid_request_error",
                "model_not_found",
                Some("model"),
            )
        }
    };
    let chat = path != "/v1/completions";
    let stream = payload["stream"] == true;
    let Some(object) = payload.as_object_mut() else {
        return context.error(
            400,
            "The request body must be a JSON object.",
            "invalid_request_error",
            "invalid_request",
            None,
        );
    };
    if object.contains_key("model") {
        object.insert("model".into(), profile["model_alias"].clone());
    }
    let responses = if path == "/v1/responses" {
        match crate::responses::request(payload) {
            Ok((chat, meta)) => {
                payload = chat;
                Some(meta)
            }
            Err(error) => {
                return context.error(
                    400,
                    &error.message,
                    "invalid_request_error",
                    error.code,
                    error.param.as_deref(),
                )
            }
        }
    } else {
        None
    };
    let object = payload.as_object_mut().expect("translated request object");
    let mut conformance_notes = Vec::new();
    let mut meta = match if chat {
        chat_request(
            object,
            manager.sampling(profile["model_alias"].as_str().unwrap_or("")),
            &mut conformance_notes,
        )
    } else {
        Ok(Meta::default())
    } {
        Ok(meta) => meta,
        Err(error) => {
            manager
                .harness
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record_compat(&conformance_notes);
            return context.error(
                400,
                &error.message,
                "invalid_request_error",
                error.code,
                error.param.as_deref(),
            );
        }
    };
    let mut compat_notes = Vec::new();
    for (key, note) in [
        ("temperature", "null_temperature_dropped"),
        ("cache_prompt", "null_cache_prompt_dropped"),
    ] {
        if object.get(key).is_some_and(Value::is_null) {
            object.shift_remove(key);
            compat_notes.push(note);
        }
    }
    if chat {
        normalize_chat(object, &mut compat_notes);
    }
    compat_notes.extend(conformance_notes);
    if let Err(error) = single_choice(object, &mut compat_notes) {
        manager
            .harness
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record_compat(&compat_notes);
        return context.error(
            400,
            &error.message,
            "invalid_request_error",
            error.code,
            error.param.as_deref(),
        );
    }
    defaults(object, &profile, chat);
    let completion = if chat {
        None
    } else {
        match Completion::request(object) {
            Ok(meta) => Some(meta),
            Err(error) => {
                return context.error(
                    400,
                    &error.message,
                    "invalid_request_error",
                    error.code,
                    error.param.as_deref(),
                )
            }
        }
    };
    let key = {
        let mut registry = manager.harness.lock().unwrap_or_else(|e| e.into_inner());
        registry.record_compat(&compat_notes);
        if chat {
            registry.observe(
                &payload,
                &profile,
                &crate::harness::settings(config),
                request_ledger::header(&headers, "user-agent").as_deref(),
            )
        } else {
            None
        }
    };
    let ledger_path = if responses.is_some() {
        "/v1/chat/completions"
    } else {
        path
    };
    let mut entry = manager
        .ledger
        .entry(ledger_path, &payload, &headers, &profile["model_alias"]);
    entry.label(&payload, &headers, config, key.as_deref());
    entry.prep(context.started.elapsed());
    let mut record = RequestRecord {
        entry: Some(entry),
        admission: None,
        status: 499,
        error: Some("client_gone_queued"),
    };
    let mut observation = key.map(|key| Observation {
        manager: manager.clone(),
        key,
        profile: profile.clone(),
        cache: Value::Null,
        prompt: Value::Null,
    });
    let (admission, backend) = match manager.admit(profile).await {
        Ok(value) => value,
        Err(error) => {
            record.fail(
                503,
                if error.contains("running") {
                    "busy"
                } else {
                    "unavailable"
                },
            );
            return context.error(
                503,
                &error,
                "local_backend_busy",
                "service_unavailable",
                None,
            );
        }
    };
    if let Some(entry) = record.entry.as_mut() {
        entry.admitted(admission.queue_seconds, admission.begin_seconds);
    }
    record.admission = Some(admission);
    record.status = 503;
    record.error = Some("client_disconnected");
    let backend_prep = Instant::now();
    let client = engine::client(engine::seconds("LOCAL_LLM_BACKEND_READ_TIMEOUT", 3600.0));
    let mut request = client
        .post(format!(
            "{}{}",
            backend.url,
            if responses.is_some() {
                "/v1/chat/completions"
            } else if chat {
                path
            } else {
                "/completion"
            }
        ))
        .bearer_auth(&backend.key)
        .json(&payload);
    if chat {
        if let Some(session) = record
            .entry
            .as_ref()
            .and_then(|e| e.row["session"].as_str())
        {
            request = request.header("x-oxe-session", session);
        }
        if let Some(turn) = record
            .entry
            .as_ref()
            .and_then(|e| e.row["turn_kind"].as_str())
        {
            request = request.header("x-oxe-turn-kind", turn);
        }
        if headers.get_all("x-oxe-snapshot").iter().any(|v| {
            v.to_str()
                .ok()
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("keep"))
        }) {
            request = request.header("x-oxe-snapshot", "keep");
        }
    }
    if let Some(entry) = record.entry.as_mut() {
        entry.backend_prep(backend_prep.elapsed());
    }
    let upstream = match request.send().await {
        Ok(reply) => reply,
        Err(_) => {
            record.fail(503, "backend_died");
            return finalize(
                context.error(
                    503,
                    "The local model is not responding. Try again in a moment.",
                    "local_backend_error",
                    "service_unavailable",
                    None,
                ),
                record,
                None,
            );
        }
    };
    let status = upstream.status().as_u16();
    record.status = status;
    if stream && status == 200 {
        let state = (
            upstream.bytes_stream(),
            StreamShim {
                buffer: Vec::new(),
                reasoning: 0,
                completion,
                responses: responses.map(crate::responses::Stream::new),
                observation,
            },
            Some(record),
            false,
        );
        let output = futures_util::stream::unfold(
            state,
            move |(mut upstream, mut shim, mut record, done)| async move {
                if done {
                    if let Some(mut record) = record.take() {
                        record.complete();
                    }
                    return None;
                }
                loop {
                    match upstream.next().await {
                        Some(Ok(bytes)) => {
                            if let Some(record) = record.as_mut() {
                                record.observe(&bytes);
                            }
                            let bytes = shim.feed(&bytes);
                            if !bytes.is_empty() {
                                return Some((
                                    Ok::<_, io::Error>(bytes),
                                    (upstream, shim, record, false),
                                ));
                            }
                        }
                        Some(Err(_)) => {
                            if let Some(record) = record.as_mut() {
                                record.fail(status, "backend_died");
                            }
                            record.take();
                            return Some((
                                Err(io::Error::other("local backend stream interrupted")),
                                (upstream, shim, record, true),
                            ));
                        }
                        None => {
                            let tail = shim.finish();
                            if let Some(observation) = shim.observation.take() {
                                observation.complete();
                            }
                            return if tail.is_empty() {
                                if let Some(mut record) = record.take() {
                                    record.complete();
                                }
                                None
                            } else {
                                Some((Ok(tail), (upstream, shim, record, true)))
                            };
                        }
                    }
                }
            },
        );
        let mut reply = context.stream(Body::from_stream(output));
        meta.headers(&mut reply);
        return reply;
    }
    let mut value = match upstream.bytes().await.ok().and_then(|bytes| {
        record.observe(&bytes);
        serde_json::from_slice::<Value>(&bytes).ok()
    }) {
        Some(value) => value,
        None => {
            record.fail(status, "backend_died");
            return finalize(
                context.error(
                    502,
                    "The local backend sent an invalid response.",
                    "local_backend_error",
                    "server_error",
                    None,
                ),
                record,
                None,
            );
        }
    };
    if let Some(observation) = observation.as_mut() {
        observation.capture(&value);
    }
    if status >= 400 {
        value = normalize_error(status, value);
    } else if let Some(completion) = completion {
        value = completion.response(value);
    } else {
        chat_response(&mut value, &mut meta);
        if let Some(responses) = responses {
            value = responses.body(value);
        }
    }
    let mut reply = context.reply(status, value);
    if status == 200 {
        meta.headers(&mut reply);
    }
    finalize(
        reply,
        record,
        if status == 200 { observation } else { None },
    )
}
