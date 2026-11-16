//! Pure Responses event translation; no transport, credentials or continuation store.
use serde_json::{json, Value};

#[derive(Debug, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

pub struct Completed {
    pub assistant: Value,
    pub native_blocks: Vec<Value>,
    pub usage: Option<Usage>,
    pub finish_reason: &'static str,
    pub failed: bool,
}

impl std::fmt::Debug for Completed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Completed")
            .field("assistant", &"[redacted]")
            .field("native_blocks", &"[redacted]")
            .field("usage", &self.usage)
            .field("finish_reason", &self.finish_reason)
            .field("failed", &self.failed)
            .finish()
    }
}

struct Item {
    native: Value,
    slot: Option<usize>,
    arguments: String,
    emitted: bool,
}

/// Text is streamed immediately. Tool arguments are buffered until item completion
/// (or response completion), since Responses can supply them only in a done event.
/// This deliberately never emits a speculative name + empty-arguments tool call.
/// Callers must redact visible chunks and keep native blocks private.
pub struct Stream {
    model: String,
    created: u64,
    include_usage: bool,
    started: bool,
    stopped: bool,
    failed: bool,
    reason: &'static str,
    usage: Option<Usage>,
    items: std::collections::BTreeMap<u64, Item>,
    fields: std::collections::BTreeMap<(u64, u64, &'static str), String>,
    next_slot: usize,
}

impl Usage {
    fn from_value(v: &Value) -> Option<Self> {
        let usage = Self {
            prompt_tokens: v["input_tokens"].as_u64()?,
            cached_tokens: v
                .get("input_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .map_or(Some(0), Value::as_u64)?,
            output_tokens: v["output_tokens"].as_u64()?,
            reasoning_tokens: v
                .get("output_tokens_details")
                .and_then(|d| d.get("reasoning_tokens"))
                .map_or(Some(0), Value::as_u64)?,
        };
        usage.prompt_tokens.checked_add(usage.output_tokens)?;
        (usage.cached_tokens <= usage.prompt_tokens && usage.reasoning_tokens <= usage.output_tokens)
            .then_some(usage)
    }

    pub fn openai(&self) -> Value {
        json!({"prompt_tokens":self.prompt_tokens, "completion_tokens":self.output_tokens,
            "total_tokens":self.prompt_tokens + self.output_tokens,
            "prompt_tokens_details":{"cached_tokens":self.cached_tokens},
            "completion_tokens_details":{"reasoning_tokens":self.reasoning_tokens}})
    }
}

impl Stream {
    pub fn new(model: impl Into<String>, created: u64, include_usage: bool) -> Self {
        Self {
            model: model.into(),
            created,
            include_usage,
            started: false,
            stopped: false,
            failed: false,
            reason: "stop",
            usage: None,
            items: Default::default(),
            fields: Default::default(),
            next_slot: 0,
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({"id":"chatcmpl_openai", "object":"chat.completion.chunk", "model":self.model,
            "created":self.created, "choices":[{"index":0, "delta":delta,"finish_reason":finish}]})
    }

    fn index(&self, v: &Value) -> Result<u64, ()> {
        if let Some(index) = v["output_index"].as_u64() {
            return Ok(index);
        }
        let id = v["item_id"].as_str().ok_or(())?;
        self.items
            .iter()
            .find_map(|(index, item)| (item.native["id"] == id).then_some(*index))
            .ok_or(())
    }

    fn item(&mut self, index: u64, native: &Value) -> Result<(), ()> {
        let object = native.as_object().ok_or(())?;
        let item = self.items.entry(index).or_insert_with(|| Item {
            native: json!({}),
            slot: None,
            arguments: String::new(),
            emitted: false,
        });
        if let Some(arguments) = native["arguments"].as_str() {
            // Empty snapshot placeholders must not erase arguments from deltas/done.
            if !arguments.is_empty() || item.arguments.is_empty() {
                if !arguments.starts_with(&item.arguments) || item.emitted && arguments != item.arguments {
                    return Err(());
                }
                item.arguments = arguments.to_owned();
            }
        }
        if item.emitted
            && object.iter().any(|(key, value)| {
                key != "arguments" && item.native.get(key).is_some_and(|old| old != value)
            })
        {
            return Err(());
        }
        item.native.as_object_mut().ok_or(())?.extend(object.clone());
        if item.native["type"] == "function_call" {
            if item.slot.is_none() {
                item.slot = Some(self.next_slot);
                self.next_slot += 1;
            }
            item.native["arguments"] = json!(item.arguments);
        }
        Ok(())
    }

    fn tool(&mut self, index: u64, out: &mut Vec<Value>) -> Result<(), ()> {
        let item = self.items.get_mut(&index).ok_or(())?;
        let Some(slot) = item.slot else {
            return Ok(());
        };
        if item.emitted {
            return Ok(());
        }
        let id = item.native["call_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(())?;
        let name = item.native["name"].as_str().filter(|s| !s.is_empty()).ok_or(())?;
        if !serde_json::from_str::<Value>(&item.arguments)
            .map_err(|_| ())?
            .is_object()
        {
            return Err(());
        }
        let call = json!({"index":slot,"id":id,"type":"function",
            "function":{"name":name,"arguments":item.arguments}});
        item.emitted = true;
        out.push(self.chunk(json!({"tool_calls":[call]}), None));
        Ok(())
    }

    fn text(
        &mut self,
        index: u64,
        part: u64,
        field: &'static str,
        text: &str,
        done: bool,
        out: &mut Vec<Value>,
    ) -> Result<(), ()> {
        let previous = self.fields.entry((index, part, field)).or_default();
        let delta = if done {
            text.strip_prefix(previous.as_str()).ok_or(())?
        } else {
            text
        };
        previous.push_str(delta);
        if !delta.is_empty() {
            let visible = if field == "reasoning_summary" {
                "reasoning_content"
            } else {
                field
            };
            out.push(self.chunk(json!({visible:delta}), None));
        }
        Ok(())
    }

    fn item_text(&mut self, index: u64, item: &Value, out: &mut Vec<Value>) -> Result<(), ()> {
        for (key, field) in [("content", "content"), ("summary", "reasoning_summary")] {
            if let Some(parts) = item[key].as_array() {
                for (part, value) in parts.iter().enumerate() {
                    let (field, text_key) = if value["type"] == "refusal" {
                        ("refusal", "refusal")
                    } else if key == "content"
                        && (item["type"] == "reasoning" || value["type"] == "reasoning_text")
                    {
                        ("reasoning_content", "text")
                    } else {
                        (field, "text")
                    };
                    if let Some(text) = value[text_key].as_str() {
                        self.text(index, part as u64, field, text, true, out)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Translate decoded events. Errors are static, terminal and never echo an
    /// upstream error body. Events after a terminal event are ignored.
    pub fn event(&mut self, value: &Value) -> Vec<Value> {
        if self.stopped || self.failed {
            return Vec::new();
        }
        match self.translate(value) {
            Ok(mut chunks) => {
                if !self.started && (!chunks.is_empty() || value["type"] == "response.created") {
                    self.started = true;
                    chunks.insert(0, self.chunk(json!({"role":"assistant","content":null}), None));
                }
                chunks
            }
            Err(()) => {
                self.failed = true;
                self.usage = None;
                vec![
                    json!({"error":{"type":"upstream_error","message":"Invalid or failed OpenAI response stream"}}),
                ]
            }
        }
    }

    fn translate(&mut self, v: &Value) -> Result<Vec<Value>, ()> {
        let kind = v["type"].as_str().ok_or(())?;
        let mut out = Vec::new();
        match kind {
            "response.failed" | "error" => return Err(()),
            "response.output_item.added" | "response.output_item.done" => {
                let index = self.index(v)?;
                self.item(index, &v["item"])?;
                if kind.ends_with(".done") {
                    self.item_text(index, &v["item"], &mut out)?;
                    // A done item can still contain token-truncated arguments.
                    // Only the terminal response tells us whether that is an error.
                    if serde_json::from_str::<Value>(&self.items[&index].arguments)
                        .is_ok_and(|v| v.is_object())
                    {
                        self.tool(index, &mut out)?;
                    }
                }
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                let index = self.index(v)?;
                let item = self.items.get_mut(&index).ok_or(())?;
                if item.slot.is_none() {
                    return Err(());
                }
                if kind.ends_with(".delta") {
                    if item.emitted {
                        return Err(());
                    }
                    item.arguments.push_str(v["delta"].as_str().ok_or(())?);
                } else {
                    let arguments = v["arguments"].as_str().ok_or(())?;
                    if !arguments.starts_with(&item.arguments) || item.emitted && arguments != item.arguments
                    {
                        return Err(());
                    }
                    item.arguments = arguments.to_owned();
                }
                item.native["arguments"] = json!(item.arguments);
            }
            "response.output_text.delta"
            | "response.output_text.done"
            | "response.refusal.delta"
            | "response.refusal.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done" => {
                let index = self.index(v)?;
                let reasoning = kind.starts_with("response.reasoning");
                let summary = kind.starts_with("response.reasoning_summary");
                let part = v[if summary { "summary_index" } else { "content_index" }]
                    .as_u64()
                    .ok_or(())?;
                let refusal = kind.starts_with("response.refusal");
                let field = if summary {
                    "reasoning_summary"
                } else if reasoning {
                    "reasoning_content"
                } else if refusal {
                    "refusal"
                } else {
                    "content"
                };
                let done = kind.ends_with(".done");
                let text_key = if !done {
                    "delta"
                } else if refusal {
                    "refusal"
                } else {
                    "text"
                };
                let text = v[text_key].as_str().ok_or(())?;
                self.text(index, part, field, text, done, &mut out)?;
            }
            "response.completed" | "response.incomplete" => {
                let response = &v["response"];
                if response["status"] == "failed" {
                    return Err(());
                }
                if let Some(items) = response["output"].as_array() {
                    for (index, item) in items.iter().enumerate() {
                        self.item(index as u64, item)?;
                        self.item_text(index as u64, item, &mut out)?;
                    }
                }
                let incomplete = kind == "response.incomplete" || response["status"] == "incomplete";
                let indices: Vec<_> = self.items.keys().copied().collect();
                for index in indices {
                    // A token-limited response may end mid-JSON. Never execute it.
                    if incomplete
                        && self.items[&index].slot.is_some()
                        && !serde_json::from_str::<Value>(&self.items[&index].arguments)
                            .is_ok_and(|v| v.is_object())
                    {
                        continue;
                    }
                    self.tool(index, &mut out)?;
                }
                self.reason = if incomplete {
                    if response["incomplete_details"]["reason"] == "content_filter" {
                        "content_filter"
                    } else {
                        "length"
                    }
                } else if self.next_slot > 0 {
                    "tool_calls"
                } else {
                    "stop"
                };
                if let Some(usage) = response.get("usage").filter(|u| !u.is_null()) {
                    self.usage = Usage::from_value(usage);
                }
                self.stopped = true;
                out.push(self.chunk(json!({}), Some(self.reason)));
                if self.include_usage {
                    if let Some(usage) = &self.usage {
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

    /// `failed` also covers EOF without a terminal event; failed completions expose
    /// no assistant/native replay state or trusted usage. No IO occurs at EOF.
    pub fn finish(self) -> Completed {
        let failed = self.failed || !self.stopped;
        if failed {
            return Completed {
                assistant: Value::Null,
                native_blocks: Vec::new(),
                usage: None,
                finish_reason: "error",
                failed: true,
            };
        }
        let text: String = self
            .fields
            .iter()
            .filter(|((_, _, field), _)| *field == "content")
            .map(|(_, v)| v.as_str())
            .collect();
        let reasoning: String = self
            .fields
            .iter()
            .filter(|((_, _, field), _)| matches!(*field, "reasoning_content" | "reasoning_summary"))
            .map(|(_, v)| v.as_str())
            .collect();
        let mut assistant =
            json!({"role":"assistant", "content":if text.is_empty() { Value::Null } else { json!(text) }});
        if !reasoning.is_empty() {
            assistant["reasoning_content"] = json!(reasoning);
        }
        let refusal: String = self
            .fields
            .iter()
            .filter(|((_, _, field), _)| *field == "refusal")
            .map(|(_, v)| v.as_str())
            .collect();
        if !refusal.is_empty() {
            assistant["refusal"] = json!(refusal);
        }
        let mut calls: Vec<_> = self.items.values().filter(|i| i.emitted).map(|i| (i.slot, json!({
            "id":i.native["call_id"], "type":"function", "function":{"name":i.native["name"], "arguments":i.arguments}
        }))).collect();
        calls.sort_by_key(|(slot, _)| *slot);
        if !calls.is_empty() {
            assistant["tool_calls"] = json!(calls.into_iter().map(|(_, call)| call).collect::<Vec<_>>());
        }
        Completed {
            assistant,
            native_blocks: self.items.into_values().map(|i| i.native).collect(),
            usage: self.usage,
            finish_reason: self.reason,
            failed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn added(index: u64, id: &str, name: &str) -> Value {
        json!({"type":"response.output_item.added", "output_index":index,
            "item":{"type":"function_call", "id":id, "call_id":format!("call_{id}"), "name":name, "arguments":""}})
    }

    fn terminal(kind: &str) -> Value {
        json!({"type":kind, "response":{"output":[]}})
    }

    fn run(events: Vec<Value>) -> (Vec<Value>, Completed) {
        let mut stream = Stream::new("test-model", 7, true);
        let chunks = events.iter().flat_map(|e| stream.event(e)).collect();
        (chunks, stream.finish())
    }

    fn assert_call(chunks: &[Value], completed: &Completed, slot: usize, name: &str, args: &str) {
        assert!(!completed.failed);
        assert_eq!(completed.finish_reason, "tool_calls");
        let call = &completed.assistant["tool_calls"][slot];
        assert_eq!(call["function"]["name"], name);
        assert_eq!(call["function"]["arguments"], args);
        let emitted: Vec<_> = chunks
            .iter()
            .flat_map(|c| {
                c["choices"][0]["delta"]["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
            })
            .filter(|c| c["index"] == slot)
            .collect();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0]["id"], call["id"]);
        assert_eq!(emitted[0]["function"], call["function"]);
    }

    #[test]
    fn arguments_only_in_response_output_item_done() {
        let (chunks, done) = run(vec![
            added(0, "fc_a", "read"),
            json!({"type":"response.output_item.done", "output_index":0,
                "item":{"type":"function_call", "id":"fc_a", "call_id":"call_fc_a", "name":"read", "arguments":"{\"path\":\"fixture\"}"}}),
            terminal("response.completed"),
        ]);
        assert_call(&chunks, &done, 0, "read", "{\"path\":\"fixture\"}");
    }

    #[test]
    fn arguments_only_in_function_call_arguments_done() {
        let (chunks, done) = run(vec![
            added(0, "fc_a", "read"),
            json!({"type":"response.function_call_arguments.done", "item_id":"fc_a", "arguments":"{\"path\":\"fixture\"}"}),
            terminal("response.completed"),
        ]);
        assert_call(&chunks, &done, 0, "read", "{\"path\":\"fixture\"}");
    }

    #[test]
    fn partial_deltas_completed_by_done() {
        for done_event in [
            json!({"type":"response.function_call_arguments.done", "item_id":"fc_a", "arguments":"{\"path\":\"fixture\"}"}),
            json!({"type":"response.output_item.done", "output_index":0,
                "item":{"type":"function_call", "id":"fc_a", "call_id":"call_fc_a", "name":"read", "arguments":"{\"path\":\"fixture\"}"}}),
        ] {
            let (chunks, done) = run(vec![
                added(0, "fc_a", "read"),
                json!({"type":"response.function_call_arguments.delta", "item_id":"fc_a", "delta":"{\"path\":"}),
                done_event,
                terminal("response.completed"),
            ]);
            assert_call(&chunks, &done, 0, "read", "{\"path\":\"fixture\"}");
        }
    }

    #[test]
    fn response_completed_with_empty_output() {
        let (chunks, done) = run(vec![
            json!({"type":"response.output_text.delta", "output_index":0, "content_index":0, "delta":"hello"}),
            terminal("response.completed"),
        ]);
        assert!(!done.failed);
        assert_eq!(done.assistant["content"], "hello");
        assert_eq!(done.finish_reason, "stop");
        assert!(chunks.iter().any(|c| c["choices"][0]["finish_reason"] == "stop"));
    }

    #[test]
    fn incomplete_finish_reason_length() {
        let (chunks, done) = run(vec![terminal("response.incomplete")]);
        assert!(!done.failed);
        assert_eq!(done.finish_reason, "length");
        assert!(chunks
            .iter()
            .any(|c| c["choices"][0]["finish_reason"] == "length"));
    }

    #[test]
    fn response_failed() {
        let mut stream = Stream::new("test-model", 7, true);
        let chunks = stream.event(&json!({"type":"response.failed", "response":{"error":{"message":"private upstream detail"}, "usage":{"input_tokens":5,"output_tokens":2}}}));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0]["error"]["type"], "upstream_error");
        assert!(!chunks[0].to_string().contains("private upstream detail"));
        assert!(stream.event(&terminal("response.completed")).is_empty());
        let done = stream.finish();
        assert!(done.failed);
        assert!(done.usage.is_none());
        assert!(done.assistant.is_null());
    }

    #[test]
    fn parallel_tool_calls_keep_arguments_and_stable_dense_slots() {
        let (chunks, done) = run(vec![
            added(1, "fc_a", "read"),
            added(3, "fc_b", "bash"),
            json!({"type":"response.function_call_arguments.delta", "item_id":"fc_b", "delta":"{\"command\":\"true\"}"}),
            json!({"type":"response.function_call_arguments.done", "item_id":"fc_a", "arguments":"{\"path\":\"fixture\"}"}),
            terminal("response.completed"),
        ]);
        assert_call(&chunks, &done, 0, "read", "{\"path\":\"fixture\"}");
        assert_call(&chunks, &done, 1, "bash", "{\"command\":\"true\"}");
        assert_ne!(
            done.assistant["tool_calls"][0]["id"],
            done.assistant["tool_calls"][1]["id"]
        );
    }

    #[test]
    fn terminal_output_fallback_and_provider_neutral_usage() {
        let (chunks, done) = run(vec![json!({"type":"response.completed", "response":{
            "output":[{"type":"function_call", "id":"fc_a", "call_id":"call_a", "name":"read", "arguments":"{}"}],
            "usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":6,"output_tokens_details":{"reasoning_tokens":2}}
        }})]);
        assert_call(&chunks, &done, 0, "read", "{}");
        assert_eq!(
            done.usage,
            Some(Usage {
                prompt_tokens: 10,
                cached_tokens: 4,
                output_tokens: 6,
                reasoning_tokens: 2
            })
        );
        let usage = &chunks.last().unwrap()["usage"];
        assert_eq!(usage["total_tokens"], 16);
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 4);
        assert_eq!(usage["completion_tokens_details"]["reasoning_tokens"], 2);
        assert_eq!(done.native_blocks.len(), 1);
    }

    #[test]
    fn text_and_reasoning_done_reconcile_without_duplication() {
        let (chunks, done) = run(vec![
            json!({"type":"response.output_text.delta", "output_index":0,"content_index":0,"delta":"hel"}),
            json!({"type":"response.output_text.done", "output_index":0,"content_index":0,"text":"hello"}),
            json!({"type":"response.reasoning_summary_text.delta", "output_index":1,"summary_index":0,"delta":"think"}),
            json!({"type":"response.completed", "response":{"output":[
                {"type":"message","content":[{"type":"output_text","text":"hello"}]},
                {"type":"reasoning","summary":[{"type":"summary_text","text":"thinking"}]}]}}),
        ]);
        assert!(!done.failed);
        assert_eq!(done.assistant["content"], "hello");
        assert_eq!(done.assistant["reasoning_content"], "thinking");
        let text: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect();
        assert_eq!(text, "hello");
    }

    #[test]
    fn truncated_item_done_preserves_incomplete_usage_without_a_tool_call() {
        for include_output in [false, true] {
            let item = json!({"type":"function_call", "id":"fc_a", "call_id":"call_fc_a",
                "name":"read", "arguments":"{\"path\":"});
            let (chunks, done) = run(vec![
                added(0, "fc_a", "read"),
                json!({"type":"response.function_call_arguments.delta", "item_id":"fc_a", "delta":"{\"path\":"}),
                json!({"type":"response.function_call_arguments.done", "item_id":"fc_a", "arguments":"{\"path\":"}),
                json!({"type":"response.output_item.done", "output_index":0, "item":item}),
                json!({"type":"response.incomplete", "response":{
                    "output":if include_output { vec![item] } else { vec![] },
                    "incomplete_details":{"reason":"max_output_tokens"},
                    "usage":{"input_tokens":10,"output_tokens":3}}}),
            ]);
            assert!(!done.failed);
            assert_eq!(done.finish_reason, "length");
            assert_eq!(done.usage.as_ref().unwrap().output_tokens, 3);
            assert!(done.assistant.get("tool_calls").is_none());
            assert!(chunks
                .iter()
                .all(|c| c.get("error").is_none() && c["choices"][0]["delta"].get("tool_calls").is_none()));
            assert!(chunks
                .iter()
                .any(|c| c["choices"][0]["finish_reason"] == "length"));
            assert_eq!(chunks.last().unwrap()["usage"]["completion_tokens"], 3);
        }
    }

    #[test]
    fn malformed_item_done_is_rejected_on_successful_terminal() {
        let mut stream = Stream::new("test-model", 7, true);
        let chunks = stream.event(&json!({"type":"response.output_item.done", "output_index":0,
            "item":{"type":"function_call", "id":"fc_a", "call_id":"call_a", "name":"read", "arguments":"{"}}));
        assert!(chunks.is_empty());
        let chunks = stream.event(&terminal("response.completed"));
        assert_eq!(chunks[0]["error"]["type"], "upstream_error");
        assert!(stream.finish().failed);
    }

    #[test]
    fn refusal_deltas_and_snapshots_preserve_separate_refusal_field() {
        for mode in ["deltas", "item_done", "terminal"] {
            let item = json!({"type":"message", "id":"msg_a", "content":[
                {"type":"output_text","text":"Visible text"},
                {"type":"refusal","refusal":"Cannot help"}]});
            let mut events = vec![];
            if mode == "deltas" {
                events.extend([
                    json!({"type":"response.refusal.delta", "output_index":0,"content_index":1,"delta":"Cannot "}),
                    json!({"type":"response.refusal.done", "output_index":0,"content_index":1,"refusal":"Cannot help"}),
                ]);
            }
            if mode != "terminal" {
                events.push(json!({"type":"response.output_item.done", "output_index":0,"item":item}));
            }
            events.push(json!({"type":"response.completed", "response":{"output":[item]}}));
            let (chunks, done) = run(events);
            assert!(!done.failed);
            assert_eq!(done.finish_reason, "stop");
            assert_eq!(done.assistant["refusal"], "Cannot help");
            assert_eq!(done.assistant["content"], "Visible text");
            let refusal: String = chunks
                .iter()
                .filter_map(|c| c["choices"][0]["delta"]["refusal"].as_str())
                .collect();
            assert_eq!(refusal, "Cannot help");
        }
    }

    #[test]
    fn refusal_only_response_is_not_an_empty_success() {
        let (chunks, done) = run(vec![json!({"type":"response.completed", "response":{"output":[
            {"type":"message", "content":[{"type":"refusal","refusal":"Cannot help"}]}]}})]);
        assert!(!done.failed);
        assert_eq!(done.assistant["refusal"], "Cannot help");
        assert!(done.assistant["content"].is_null());
        assert!(chunks
            .iter()
            .any(|c| c["choices"][0]["delta"]["refusal"] == "Cannot help"));
    }

    #[test]
    fn incomplete_content_filter_finish_reason() {
        for kind in ["response.incomplete", "response.completed"] {
            let (chunks, done) = run(vec![json!({"type":kind, "response":{
                "status":"incomplete", "incomplete_details":{"reason":"content_filter"}}})]);
            assert!(!done.failed);
            assert_eq!(done.finish_reason, "content_filter");
            assert!(chunks
                .iter()
                .any(|c| c["choices"][0]["finish_reason"] == "content_filter"));
        }
    }

    #[test]
    fn eof_without_terminal_is_failed() {
        assert!(Stream::new("test-model", 7, false).finish().failed);
    }
    #[test]
    fn invalid_terminal_usage_keeps_output_with_unknown_metering() {
        for usage in [
            json!({"input_tokens":10,"output_tokens":"invalid"}),
            json!({"input_tokens":1,"output_tokens":2,"input_tokens_details":{"cached_tokens":3}}),
        ] {
            let (chunks, done) = run(vec![
                json!({"type":"response.completed","response":{"output":[{"type":"message","content":[{"type":"output_text","text":"answer"}]}],"usage":usage}}),
            ]);
            assert!(!done.failed);
            assert_eq!(done.assistant["content"], "answer");
            assert!(done.usage.is_none());
            assert!(chunks.iter().all(|v| v.get("usage").is_none()));
        }
    }
    #[test]
    fn reasoning_text_events_and_snapshots_never_enter_answer_content() {
        let item = json!({"type":"reasoning","id":"rs_synthetic","encrypted_content":"opaque-synthetic-private","content":[{"type":"reasoning_text","text":"private reasoning"}]});
        let (chunks, done) = run(vec![
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_synthetic"}}),
            json!({"type":"response.reasoning_text.delta","output_index":0,"content_index":0,"delta":"private "}),
            json!({"type":"response.reasoning_text.done","output_index":0,"content_index":0,"text":"private reasoning"}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            terminal("response.completed"),
        ]);
        assert!(!done.failed);
        assert!(done.assistant["content"].is_null());
        assert_eq!(done.assistant["reasoning_content"], "private reasoning");
        let streamed: String = chunks
            .iter()
            .filter_map(|c| c.pointer("/choices/0/delta/reasoning_content")?.as_str())
            .collect();
        assert_eq!(streamed, "private reasoning");
        assert!(!serde_json::to_string(&chunks)
            .unwrap()
            .contains("opaque-synthetic-private"));
    }
    #[test]
    fn opaque_output_items_remain_ordered_and_exact_with_empty_completed_output() {
        let items = vec![
            json!({"type":"reasoning","id":"rs_synthetic","encrypted_content":"opaque-synthetic-private","summary":[]}),
            json!({"type":"function_call","id":"fc_synthetic","call_id":"call_synthetic","name":"read","arguments":"{}"}),
            json!({"type":"message","id":"msg_synthetic","content":[{"type":"output_text","text":"answer"}]}),
        ];
        let mut events = vec![];
        for index in [2, 0, 1] {
            events.push(json!({"type":"response.output_item.done","output_index":index,"item":items[index]}));
        }
        events.push(terminal("response.completed"));
        let (chunks, done) = run(events);
        assert!(!done.failed);
        assert_eq!(done.native_blocks, items);
        assert!(!serde_json::to_string(&chunks)
            .unwrap()
            .contains("opaque-synthetic-private"));
    }

    #[test]
    fn completed_debug_and_errors_hide_opaque_items() {
        let item = json!({"type":"reasoning","id":"rs_synthetic","encrypted_content":"opaque-synthetic-private","summary":[]});
        let (_, done) = run(vec![
            json!({"type":"response.completed","response":{"output":[item]}}),
        ]);
        assert!(!done.failed);
        assert!(!format!("{done:?}").contains("opaque-synthetic-private"));
        let (chunks, failed) = run(vec![
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            json!({"type":"response.failed","response":{"error":{"message":"opaque-synthetic-private"}}}),
        ]);
        assert!(failed.failed);
        assert!(failed.native_blocks.is_empty());
        assert!(!format!("{failed:?} {chunks:?}").contains("opaque-synthetic-private"));
    }
    #[test]
    fn summary_and_full_reasoning_use_independent_snapshot_channels() {
        let (_, done) = run(vec![
            json!({"type":"response.completed","response":{"output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"summary"}],"content":[{"type":"reasoning_text","text":"full reasoning"}]}]}}),
        ]);
        assert!(!done.failed);
        assert!(done.assistant["content"].is_null());
        assert!(done.assistant["reasoning_content"]
            .as_str()
            .unwrap()
            .contains("full reasoning"));
    }
    #[test]
    fn completed_arguments_cannot_rewrite_an_already_received_prefix() {
        for item_done in [false, true] {
            let mut events = vec![
                added(0, "synthetic", "read"),
                json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":"}),
            ];
            if item_done {
                events.push(json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"synthetic","call_id":"call_synthetic","name":"read","arguments":"{\"other\":1}"}}));
            } else {
                events.push(json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"other\":1}"}));
            }
            events.push(terminal("response.completed"));
            let (_, done) = run(events);
            assert!(done.failed);
            assert!(done.native_blocks.is_empty());
        }
    }
}
