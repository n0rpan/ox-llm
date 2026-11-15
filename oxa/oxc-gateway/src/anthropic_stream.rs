//! Parsed Anthropic Messages events -> OpenAI chunks and a terminal usage record.
//! SSE framing, transport, redactor implementation and accounting belong to the gateway.
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamError;

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid or incomplete Anthropic stream")
    }
}

impl std::error::Error for StreamError {}

/// Raw billing categories stay separate. Cache writes must never be priced as reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    pub fn from_value(value: &Value) -> Option<Self> {
        let usage = Self {
            input_tokens: value["input_tokens"].as_u64()?,
            output_tokens: value["output_tokens"].as_u64()?,
            cache_read_input_tokens: value
                .get("cache_read_input_tokens")
                .map_or(Some(0), Value::as_u64)?,
            cache_creation_input_tokens: value
                .get("cache_creation_input_tokens")
                .map_or(Some(0), Value::as_u64)?,
        };
        usage
            .input_tokens
            .checked_add(usage.cache_read_input_tokens)?
            .checked_add(usage.cache_creation_input_tokens)?
            .checked_add(usage.output_tokens)?;
        Some(usage)
    }

    pub fn openai(self) -> Value {
        let prompt = self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens;
        json!({"prompt_tokens":prompt, "completion_tokens":self.output_tokens,
            "total_tokens":prompt + self.output_tokens,
            "prompt_tokens_details":{"cached_tokens":self.cache_read_input_tokens,
                "cache_write_tokens":self.cache_creation_input_tokens},
            "completion_tokens_details":{"reasoning_tokens":0}})
    }
}

pub fn finish_reason(reason: &str) -> &'static str {
    match reason {
        "max_tokens" | "model_context_window_exceeded" => "length",
        "tool_use" => "tool_calls",
        "refusal" => "content_filter",
        _ => "stop",
    }
}

/// Available only after a complete, non-error stream. Native blocks can be kept
/// by the caller for exact-prefix thinking replay; they must not be logged.
pub struct Completed {
    pub assistant: Value,
    pub native_blocks: Vec<Value>,
    pub usage: Option<Usage>,
    pub finish_reason: &'static str,
}

struct Block {
    native: Value,
    slot: Option<usize>,
    arguments: String,
    safe_arguments: String,
    safe_id: String,
    safe_name: String,
    closed: bool,
}

/// Each visible field has independent hold-back state; tool argument streams are
/// keyed by their stable OpenAI slot, never mixed with names or another call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Field {
    Content,
    Thinking,
    ToolArguments(usize),
}

/// Inject the gateway's streaming redactor without passing credentials here.
/// `feed` may withhold partial matches; `finish` releases a field's remaining tail.
/// `whole` redacts atomic tool names/IDs in place, never with hold-back buffering.
pub trait Redactor {
    fn feed(&mut self, field: Field, fragment: &str) -> String;
    fn finish(&mut self, field: Field) -> String;
    fn whole(&mut self, value: &str) -> String;
}

/// Explicit identity transform for callers without sensitive data and parity tests.
pub struct Identity;
impl Redactor for Identity {
    fn feed(&mut self, _: Field, fragment: &str) -> String {
        fragment.to_owned()
    }
    fn finish(&mut self, _: Field) -> String {
        String::new()
    }
    fn whole(&mut self, value: &str) -> String {
        value.to_owned()
    }
}

pub struct Stream<R: Redactor = Identity> {
    redactor: R,
    model: String,
    created: u64,
    include_usage: bool,
    started: bool,
    stopped: bool,
    failed: bool,
    finish: Option<&'static str>,
    raw_usage: Map<String, Value>,
    usage_final: bool,
    blocks: BTreeMap<u64, Block>,
    next_slot: usize,
    text: String,
    reasoning: String,
}

impl Stream<Identity> {
    /// The caller supplies one timestamp so the translator needs no clock or IO.
    pub fn new(model: impl Into<String>, created: u64, include_usage: bool) -> Self {
        Self::with_redactor(model, created, include_usage, Identity)
    }
}

impl<R: Redactor> Stream<R> {
    /// Production gateways supply their stateful redactor here. Completed.assistant
    /// is assembled from the same safe fragments as emitted chunks. Native blocks
    /// stay opaque/unmodified and must remain inside the exact-prefix private store.
    pub fn with_redactor(model: impl Into<String>, created: u64, include_usage: bool, redactor: R) -> Self {
        Self {
            redactor,
            model: model.into(),
            created,
            include_usage,
            started: false,
            stopped: false,
            failed: false,
            finish: None,
            raw_usage: Map::new(),
            usage_final: false,
            blocks: BTreeMap::new(),
            next_slot: 0,
            text: String::new(),
            reasoning: String::new(),
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({"id":"chatcmpl_anthropic", "object":"chat.completion.chunk", "created":self.created,
            "model":self.model, "choices":[{"index":0, "delta":delta, "finish_reason":finish}]})
    }

    fn usage(&self) -> Option<Usage> {
        self.usage_final
            .then(|| Usage::from_value(&Value::Object(self.raw_usage.clone())))
            .flatten()
    }

    fn merge_usage(&mut self, value: &Value) -> Result<(), StreamError> {
        self.raw_usage
            .extend(value.as_object().ok_or(StreamError)?.clone());
        Ok(())
    }

    /// Translate one decoded event. Once rejected, the stream stays failed.
    pub fn event(&mut self, value: &Value) -> Result<Vec<Value>, StreamError> {
        if self.failed {
            return Err(StreamError);
        }
        if self.stopped {
            return Ok(Vec::new());
        }
        let result = self.translate(value);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn translate(&mut self, value: &Value) -> Result<Vec<Value>, StreamError> {
        let kind = value["type"].as_str().ok_or(StreamError)?;
        if kind == "error" {
            return Err(StreamError);
        }
        if kind == "ping" {
            return Ok(Vec::new());
        }
        if kind == "message_start" {
            if self.started {
                return Err(StreamError);
            }
            self.started = true;
            if let Some(usage) = value["message"].get("usage") {
                self.merge_usage(usage)?;
            }
            return Ok(vec![self.chunk(json!({"role":"assistant", "content":null}), None)]);
        }
        if !self.started {
            return Err(StreamError);
        }
        let mut out = Vec::new();
        match kind {
            "content_block_start" => {
                let index = value["index"].as_u64().ok_or(StreamError)?;
                if self.blocks.contains_key(&index) {
                    return Err(StreamError);
                }
                let mut native = value["content_block"].as_object().ok_or(StreamError)?.clone();
                let (mut safe_id, mut safe_name) = (String::new(), String::new());
                let slot = match native.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let slot = self.next_slot;
                        self.next_slot += 1;
                        let id = native.entry("id").or_insert(json!("toolu_unknown"));
                        if !id.is_string() {
                            return Err(StreamError);
                        }
                        let name = native.entry("name").or_insert(json!("tool"));
                        if !name.is_string() {
                            return Err(StreamError);
                        }
                        native.entry("input").or_insert(json!({}));
                        safe_id = self.redactor.whole(native["id"].as_str().ok_or(StreamError)?);
                        safe_name = self.redactor.whole(native["name"].as_str().ok_or(StreamError)?);
                        out.push(self.chunk(
                            json!({"tool_calls":[{"index":slot, "id":safe_id,
                            "type":"function", "function":{"name":safe_name, "arguments":""}}]}),
                            None,
                        ));
                        Some(slot)
                    }
                    Some("thinking") => {
                        native.entry("thinking").or_insert(json!(""));
                        native.entry("signature").or_insert(json!(""));
                        None
                    }
                    Some("text") => {
                        native.entry("text").or_insert(json!(""));
                        None
                    }
                    Some(_) => None,
                    None => return Err(StreamError),
                };
                self.blocks.insert(
                    index,
                    Block {
                        native: Value::Object(native),
                        slot,
                        arguments: String::new(),
                        safe_arguments: String::new(),
                        safe_id,
                        safe_name,
                        closed: false,
                    },
                );
            }
            "content_block_delta" => {
                let index = value["index"].as_u64().ok_or(StreamError)?;
                let block = self
                    .blocks
                    .get_mut(&index)
                    .filter(|b| !b.closed)
                    .ok_or(StreamError)?;
                let delta = &value["delta"];
                let field = match delta["type"].as_str() {
                    Some("text_delta") => Some(("text", "content")),
                    Some("thinking_delta") => Some(("thinking", "reasoning_content")),
                    Some("signature_delta") => Some(("signature", "")),
                    Some("input_json_delta") => Some(("partial_json", "tool_calls")),
                    _ => None,
                };
                if let Some((field, wire)) = field {
                    let text = delta[field].as_str().ok_or(StreamError)?;
                    if field == "partial_json" {
                        let slot = block.slot.ok_or(StreamError)?;
                        block.arguments.push_str(text);
                        let safe = self.redactor.feed(Field::ToolArguments(slot), text);
                        block.safe_arguments.push_str(&safe);
                        if !safe.is_empty() {
                            out.push(self.chunk(
                                json!({"tool_calls":[{"index":slot,
                                "function":{"arguments":safe}}]}),
                                None,
                            ));
                        }
                    } else {
                        if block.native["type"] != if field == "text" { "text" } else { "thinking" } {
                            return Err(StreamError);
                        }
                        if field == "signature" {
                            // Match the reference's opaque replacement semantics.
                            block.native[field] = json!(text);
                            return Ok(out);
                        }
                        let existing = block.native[field].as_str().ok_or(StreamError)?;
                        block.native[field] = json!(format!("{existing}{text}"));
                        let safe = self.redactor.feed(
                            if field == "text" {
                                Field::Content
                            } else {
                                Field::Thinking
                            },
                            text,
                        );
                        if field == "text" {
                            self.text.push_str(&safe);
                        }
                        if field == "thinking" {
                            self.reasoning.push_str(&safe);
                        }
                        if !safe.is_empty() {
                            out.push(self.chunk(json!({wire:safe}), None));
                        }
                    }
                }
            }
            "content_block_stop" => {
                let index = value["index"].as_u64().ok_or(StreamError)?;
                let block = self
                    .blocks
                    .get_mut(&index)
                    .filter(|b| !b.closed)
                    .ok_or(StreamError)?;
                block.closed = true;
                if block.slot.is_some() {
                    block.native["input"] = if block.arguments.is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&block.arguments)
                            .unwrap_or_else(|_| json!({"input":block.arguments}))
                    };
                }
            }
            "message_delta" => {
                self.usage_final = false;
                if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                    self.finish = Some(finish_reason(reason));
                }
                if let Some(usage) = value.get("usage") {
                    self.merge_usage(usage)?;
                    self.usage_final = usage["output_tokens"].as_u64().is_some()
                        && Usage::from_value(&Value::Object(self.raw_usage.clone())).is_some();
                }
            }
            "message_stop" => {
                if self.blocks.values().any(|b| !b.closed) {
                    return Err(StreamError);
                }
                self.stopped = true;
                let tail = self.redactor.finish(Field::Content);
                self.text.push_str(&tail);
                if !tail.is_empty() {
                    out.push(self.chunk(json!({"content":tail}), None));
                }
                let tail = self.redactor.finish(Field::Thinking);
                self.reasoning.push_str(&tail);
                if !tail.is_empty() {
                    out.push(self.chunk(json!({"reasoning_content":tail}), None));
                }
                let mut tails = Vec::new();
                for block in self.blocks.values_mut() {
                    if let Some(slot) = block.slot {
                        let tail = self.redactor.finish(Field::ToolArguments(slot));
                        block.safe_arguments.push_str(&tail);
                        if !tail.is_empty() {
                            tails.push((slot, tail));
                        }
                    }
                }
                for (slot, tail) in tails {
                    out.push(self.chunk(
                        json!({"tool_calls":[{"index":slot,"function":{"arguments":tail}}]}),
                        None,
                    ));
                }
                out.push(self.chunk(json!({}), Some(self.finish.unwrap_or("stop"))));
                if self.include_usage {
                    if let Some(usage) = self.usage() {
                        let mut chunk = self.chunk(json!({}), None);
                        chunk["choices"] = json!([]);
                        chunk["usage"] = usage.openai();
                        out.push(chunk);
                    }
                }
            }
            _ => {}
        }
        Ok(out)
    }

    /// EOF without message_stop (or any prior error) cannot produce trusted usage.
    pub fn finish(self) -> Result<Completed, StreamError> {
        if !self.stopped || self.failed {
            return Err(StreamError);
        }
        let usage = self.usage();
        let mut assistant = json!({"role":"assistant", "content":if self.text.is_empty() { Value::Null } else { json!(self.text) }});
        if !self.reasoning.is_empty() {
            assistant["reasoning_content"] = json!(self.reasoning);
        }
        let mut calls: Vec<_> = self
            .blocks
            .values()
            .filter_map(|b| b.slot.map(|s| (s, b)))
            .collect();
        calls.sort_by_key(|(slot, _)| *slot);
        if !calls.is_empty() {
            assistant["tool_calls"] = calls
                .iter()
                .map(|(_, b)| {
                    json!({"id":b.safe_id,
                "type":"function", "function":{"name":b.safe_name, "arguments":b.safe_arguments}})
                })
                .collect();
        }
        Ok(Completed {
            assistant,
            native_blocks: self.blocks.into_values().map(|b| b.native).collect(),
            usage,
            finish_reason: self.finish.unwrap_or("stop"),
        })
    }
}
