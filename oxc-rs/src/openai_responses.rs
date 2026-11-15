//! OpenAI Responses request/event translation and bounded exact-prefix continuation.
use crate::openai_contract::{effort, output_limit, single_choice, upstream, validate, Error};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet, VecDeque};

struct Entry {
    key: [u8; 32],
    expires: u64,
    frozen: Vec<u8>,
}

/// Opaque provider reasoning is held only in bounded process memory. The caller's
/// scope MUST partition upstream endpoint and credential identity (not a raw key).
/// `now` is a monotonic time in seconds supplied by the caller. No IO or clock here.
pub struct HistoryStore {
    entries: VecDeque<Entry>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
    ttl: u64,
}

impl Default for HistoryStore {
    fn default() -> Self {
        Self::with_limits(128, 8 * 1024 * 1024, 1800)
    }
}

fn canonical(message: &Value) -> Value {
    let mut content = message["content"].clone();
    if let Some(parts) = content
        .as_array()
        .filter(|p| p.iter().all(|v| v["type"] == "text"))
    {
        content = json!(parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<String>());
    }
    if content == "" || content.as_array().is_some_and(|a| a.is_empty()) {
        content = Value::Null;
    }
    let calls: Vec<_> = message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|call| {
            let raw = &call["function"]["arguments"];
            let args = raw
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| raw.clone());
            json!([call["id"], call["function"]["name"], args])
        })
        .collect();
    json!({"role":message["role"], "content":content, "calls":calls, "tool_call_id":message["tool_call_id"]})
}

fn history_key(scope: &str, model: &str, history: &[Value]) -> [u8; 32] {
    let mut material = json!([
        scope,
        model,
        history.iter().map(canonical).collect::<Vec<_>>()
    ]);
    material.sort_all_objects();
    Sha256::digest(serde_json::to_vec(&material).expect("JSON values serialize")).into()
}

impl HistoryStore {
    pub fn with_limits(max_entries: usize, max_bytes: usize, ttl_seconds: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            max_entries,
            max_bytes,
            ttl: ttl_seconds,
        }
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn byte_len(&self) -> usize {
        self.bytes
    }
    /// Retain only completed output, keyed to the final redacted assistant the
    /// client receives. Incomplete or failed replies never become continuations.
    pub fn remember_completed(
        &mut self,
        scope: &str,
        model: &str,
        history: &[Value],
        terminal: &Value,
        redacted_assistant: &Value,
        now: u64,
    ) -> Result<(), Error> {
        response(terminal, model, 0)?;
        if terminal["status"] == "completed" {
            self.remember(
                scope,
                model,
                history,
                redacted_assistant,
                terminal["output"].as_array().ok_or_else(upstream)?,
                now,
            );
        }
        Ok(())
    }
    fn prune(&mut self, now: u64) {
        self.entries.retain(|entry| entry.expires >= now);
        self.bytes = self.entries.iter().map(|e| e.frozen.len()).sum();
    }
    pub fn remember(
        &mut self,
        scope: &str,
        model: &str,
        history: &[Value],
        assistant: &Value,
        output: &[Value],
        now: u64,
    ) {
        self.prune(now);
        let mut prefix = history.to_vec();
        prefix.push(assistant.clone());
        let key = history_key(scope, model, &prefix);
        if let Some(index) = self.entries.iter().position(|e| e.key == key) {
            self.bytes -= self.entries.remove(index).unwrap().frozen.len();
        }
        let frozen = serde_json::to_vec(output).expect("JSON values serialize");
        if self.max_entries == 0 || frozen.len() > self.max_bytes {
            return;
        }
        self.bytes += frozen.len();
        self.entries.push_back(Entry {
            key,
            expires: now.saturating_add(self.ttl),
            frozen,
        });
        while self.entries.len() > self.max_entries || self.bytes > self.max_bytes {
            self.bytes -= self.entries.pop_front().unwrap().frozen.len();
        }
    }
    fn restore(
        &mut self,
        scope: &str,
        model: &str,
        history: &[Value],
        now: u64,
    ) -> Option<Vec<Value>> {
        self.prune(now);
        let key = history_key(scope, model, history);
        let index = self.entries.iter().position(|e| e.key == key)?;
        let entry = self.entries.remove(index)?;
        let output = serde_json::from_slice(&entry.frozen).ok();
        self.entries.push_back(entry);
        output
    }
}

fn closed_historical_calls(messages: &[Value], index: usize) -> bool {
    let later = &messages[index + 1..];
    if !later
        .iter()
        .any(|m| matches!(m["role"].as_str(), Some("user" | "assistant")))
    {
        return false;
    }
    let mut pending: HashSet<_> = messages[index]["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["id"].as_str())
        .collect();
    for message in later {
        if matches!(message["role"].as_str(), Some("user" | "assistant")) {
            break;
        }
        if message["role"] == "tool" {
            if let Some(id) = message["tool_call_id"].as_str() {
                pending.remove(id);
            }
        }
    }
    pending.is_empty()
}

/// Translate after shape checks; a missing *open* continuation fails clearly.
/// Closed historical tool steps replay visible calls/outputs without reasoning (#251).
pub fn request(
    payload: &Value,
    model: &str,
    maximum: Option<u64>,
    scope: &str,
    store: &mut HistoryStore,
    now: u64,
) -> Result<Value, Error> {
    validate(payload)?;
    let limit = output_limit(payload, maximum)?;
    single_choice(payload)?;
    let none_supported = match model {
        "gpt-6-astra" | "gpt-6.1-sol" => false,
        "gpt-6.1-terra" | "gpt-6-sol" | "gpt-6-luna" => true,
        _ => {
            return Err(Error {
                status: 400,
                code: "unsupported_model",
                param: "model",
            })
        }
    };
    let mut effort = effort(payload)?.unwrap_or("medium");
    if effort == "minimal" || (effort == "none" && !none_supported) {
        effort = "low";
    }
    let empty = Vec::new();
    let messages = payload["messages"].as_array().unwrap_or(&empty);
    let mut inputs = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let role = &message["role"];
        let mut replay = false;
        if role == "assistant" {
            if let Some(restored) = store.restore(scope, model, &messages[..=index], now) {
                inputs.extend(restored);
                continue;
            }
            if message["tool_calls"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
            {
                if !closed_historical_calls(messages, index) {
                    return Err(Error {
                        status: 409,
                        code: "cloud_continuation_unavailable",
                        param: "messages",
                    });
                }
                replay = true;
            }
        }
        let content = &message["content"];
        if role == "tool" {
            let output = content.as_str().map(str::to_owned).unwrap_or_else(|| {
                content
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p["text"].as_str())
                    .collect()
            });
            inputs.push(json!({"type":"function_call_output", "call_id":message["tool_call_id"], "output":output}));
            continue;
        }
        if content.is_string() {
            inputs.push(json!({"role":role, "content":content}));
        } else if let Some(parts) = content.as_array() {
            let parts: Vec<_> = parts.iter().map(|p| if p["type"] == "text" {
                json!({"type":if role == "assistant" { "output_text" } else { "input_text" }, "text":p["text"]})
            } else {
                json!({"type":"input_image", "image_url":p["image_url"]["url"],
                    "detail":p["image_url"].get("detail").cloned().unwrap_or(json!("auto"))})
            }).collect();
            inputs.push(json!({"role":role, "content":parts}));
        }
        if replay {
            for call in message["tool_calls"].as_array().unwrap() {
                inputs.push(json!({"type":"function_call", "call_id":call["id"], "name":call["function"]["name"],
                    "arguments":call["function"]["arguments"], "status":"completed"}));
            }
        }
    }
    let mut body = json!({"model":model, "input":inputs, "store":false,
        "include":["reasoning.encrypted_content"], "reasoning":{"effort":effort},
        "max_output_tokens":limit, "stream":payload["stream"] == true});
    let tools: Vec<_> = payload["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|tool| {
            let mut result = json!({"type":"function"});
            for name in ["name", "description", "parameters", "strict"] {
                if let Some(value) = tool["function"].get(name) {
                    result[name] = value.clone();
                }
            }
            if result.get("strict").is_none() {
                result["strict"] = json!(false);
            }
            result
        })
        .collect();
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        let choice = payload.get("tool_choice").cloned().unwrap_or(json!("auto"));
        body["tool_choice"] = if choice.is_object() {
            json!({"type":"function", "name":choice["function"]["name"]})
        } else {
            choice
        };
        if let Some(value) = payload.get("parallel_tool_calls") {
            body["parallel_tool_calls"] = value.clone();
        }
    }
    if effort == "none" {
        for name in ["temperature", "top_p", "top_logprobs"] {
            if let Some(value) = payload.get(name) {
                body[name] = value.clone();
            }
        }
    }
    let format = &payload["response_format"];
    if format["type"] == "json_schema" {
        let mut schema = json!({"type":"json_schema"});
        if let Some(fields) = format["json_schema"].as_object() {
            schema.as_object_mut().unwrap().extend(fields.clone());
        }
        body["text"] = json!({"format":schema});
    } else if matches!(format["type"].as_str(), Some("text" | "json_object")) {
        body["text"] = json!({"format":format});
    }
    Ok(body)
}

pub fn usage(value: &Value) -> Option<Value> {
    let u = value.get("usage")?.as_object()?;
    let input = u.get("input_tokens")?.as_u64()?;
    let output = u.get("output_tokens")?.as_u64()?;
    Some(
        json!({"prompt_tokens":input, "completion_tokens":output, "total_tokens":input.checked_add(output)?,
        "prompt_tokens_details":u.get("input_tokens_details").filter(|v| !v.is_null() && **v != json!({}))
            .cloned().unwrap_or(json!({"cached_tokens":0})),
        "completion_tokens_details":u.get("output_tokens_details").filter(|v| !v.is_null() && **v != json!({}))
            .cloned().unwrap_or(json!({"reasoning_tokens":0}))}),
    )
}

/// Normalize a terminal response. Use `HistoryStore::remember_completed` to keep
/// its native output under the final redacted assistant that the client receives.
/// `history` and the returned assistant must be the same redacted values the client
/// echoes; the gateway handles redaction before calling this pure module.
pub fn response(value: &Value, public_model: &str, created: u64) -> Result<Value, Error> {
    let status = value["status"].as_str().ok_or_else(upstream)?;
    if !matches!(status, "completed" | "incomplete") {
        return Err(upstream());
    }
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut refusal = String::new();
    let mut calls = Vec::new();
    for part in value["output"].as_array().ok_or_else(upstream)? {
        match part["type"].as_str() {
            Some("message") => {
                for block in part["content"].as_array().ok_or_else(upstream)? {
                    if block["type"] == "output_text" {
                        text.push_str(block["text"].as_str().ok_or_else(upstream)?);
                    }
                    if block["type"] == "refusal" {
                        refusal.push_str(block["refusal"].as_str().ok_or_else(upstream)?);
                    }
                }
            }
            Some("function_call") => {
                for field in ["call_id", "name", "arguments"] {
                    if !part[field].is_string() {
                        return Err(upstream());
                    }
                }
                calls.push(json!({"id":part["call_id"], "type":"function", "function":{"name":part["name"], "arguments":part["arguments"]}}));
            }
            Some("reasoning") => {
                for block in part["summary"].as_array().into_iter().flatten() {
                    reasoning.push_str(block["text"].as_str().ok_or_else(upstream)?);
                }
            }
            _ => {}
        }
    }
    let mut assistant = json!({"role":"assistant", "content":if text.is_empty() { Value::Null } else { json!(text) }});
    let mut finish = if calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    if !calls.is_empty() {
        assistant["tool_calls"] = json!(calls);
    }
    if !reasoning.is_empty() {
        assistant["reasoning_content"] = json!(reasoning);
    }
    if !refusal.is_empty() {
        assistant["refusal"] = json!(refusal);
    }
    if status == "incomplete" {
        finish = if value["incomplete_details"]["reason"] == "max_output_tokens" {
            "length"
        } else {
            "content_filter"
        };
    }
    let mut out = json!({"id":value.get("id").cloned().unwrap_or(json!("chatcmpl_openai")),
        "object":"chat.completion", "created":value.get("created_at").cloned().unwrap_or(json!(created)), "model":public_model,
        "choices":[{"index":0, "message":assistant, "finish_reason":finish}]});
    if let Some(usage) = usage(value) {
        out["usage"] = usage;
    }
    Ok(out)
}

/// Parsed Responses events -> OpenAI chunks. Pass each terminal native output to
/// `remember` with the final redacted assistant, after `finish` succeeds.
pub struct Stream {
    model: String,
    created: u64,
    slots: BTreeMap<u64, usize>,
    done: bool,
    failed: bool,
    terminal: Option<Value>,
}

impl Stream {
    pub fn new(model: impl Into<String>, created: u64) -> Self {
        Self {
            model: model.into(),
            created,
            slots: BTreeMap::new(),
            done: false,
            failed: false,
            terminal: None,
        }
    }
    fn wire(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({"id":"chatcmpl_openai", "object":"chat.completion.chunk", "created":self.created,
            "model":self.model, "choices":[{"index":0, "delta":delta, "finish_reason":finish}]})
    }
    pub fn event(&mut self, value: &Value) -> Result<Vec<Value>, Error> {
        if self.failed {
            return Err(upstream());
        }
        if self.done {
            return Ok(Vec::new());
        }
        let result = self.translate(value);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn translate(&mut self, value: &Value) -> Result<Vec<Value>, Error> {
        match value["type"].as_str().ok_or_else(upstream)? {
            "response.created" => Ok(vec![
                self.wire(json!({"role":"assistant", "content":""}), None)
            ]),
            "response.output_item.added" if value["item"]["type"] == "function_call" => {
                let index = value["output_index"].as_u64().ok_or_else(upstream)?;
                let part = &value["item"];
                if self.slots.contains_key(&index)
                    || !part["call_id"].is_string()
                    || !part["name"].is_string()
                {
                    return Err(upstream());
                }
                let slot = self.slots.len();
                self.slots.insert(index, slot);
                Ok(vec![self.wire(
                    json!({"tool_calls":[{"index":slot, "id":part["call_id"], "type":"function",
                    "function":{"name":part["name"], "arguments":""}}]}),
                    None,
                )])
            }
            "response.function_call_arguments.delta" => {
                let slot = self
                    .slots
                    .get(&value["output_index"].as_u64().ok_or_else(upstream)?)
                    .ok_or_else(upstream)?;
                let delta = value["delta"].as_str().ok_or_else(upstream)?;
                Ok(vec![self.wire(
                    json!({"tool_calls":[{"index":slot,"function":{"arguments":delta}}]}),
                    None,
                )])
            }
            kind @ ("response.output_text.delta"
            | "response.refusal.delta"
            | "response.reasoning_summary_text.delta") => {
                let field = match kind {
                    "response.output_text.delta" => "content",
                    "response.refusal.delta" => "refusal",
                    _ => "reasoning_content",
                };
                Ok(vec![self.wire(
                    json!({field:value["delta"].as_str().ok_or_else(upstream)?}),
                    None,
                )])
            }
            "response.completed" | "response.incomplete" => {
                let reply = response(&value["response"], &self.model, self.created)?;
                let mut out =
                    vec![self.wire(json!({}), reply["choices"][0]["finish_reason"].as_str())];
                if let Some(usage) = reply.get("usage") {
                    let mut chunk = self.wire(json!({}), None);
                    chunk["choices"] = json!([]);
                    chunk["usage"] = usage.clone();
                    out.push(chunk);
                }
                self.done = true;
                self.terminal = Some(value["response"].clone());
                Ok(out)
            }
            "response.failed" | "error" => Err(upstream()),
            _ => Ok(Vec::new()),
        }
    }
    /// Terminal provider response (including native reasoning), or an EOF error.
    /// The gateway emits [DONE] only after this succeeds, and remembers completed
    /// output using its redacted client-visible assistant and exact history prefix.
    pub fn finish(self) -> Result<Value, Error> {
        if self.failed || !self.done {
            return Err(upstream());
        }
        self.terminal.ok_or_else(upstream)
    }
}
