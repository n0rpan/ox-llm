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
    fields: std::collections::BTreeMap<(u64, u64, bool), String>,
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
                if item.emitted && arguments != item.arguments {
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
        reasoning: bool,
        text: &str,
        done: bool,
        out: &mut Vec<Value>,
    ) -> Result<(), ()> {
        let previous = self.fields.entry((index, part, reasoning)).or_default();
        let delta = if done {
            text.strip_prefix(previous.as_str()).ok_or(())?
        } else {
            text
        };
        previous.push_str(delta);
        if !delta.is_empty() {
            let key = if reasoning { "reasoning_content" } else { "content" };
            out.push(self.chunk(json!({key:delta}), None));
        }
        Ok(())
    }

    fn item_text(&mut self, index: u64, item: &Value, out: &mut Vec<Value>) -> Result<(), ()> {
        for (key, reasoning) in [("content", false), ("summary", true)] {
            if let Some(parts) = item[key].as_array() {
                for (part, value) in parts.iter().enumerate() {
                    if let Some(text) = value["text"].as_str() {
                        self.text(index, part as u64, reasoning, text, true, out)?;
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
                    self.tool(index, &mut out)?;
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
                    if item.emitted && arguments != item.arguments {
                        return Err(());
                    }
                    item.arguments = arguments.to_owned();
                }
                item.native["arguments"] = json!(item.arguments);
            }
            "response.output_text.delta"
            | "response.output_text.done"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done" => {
                let index = self.index(v)?;
                let reasoning = kind.starts_with("response.reasoning");
                let part = v[if reasoning {
                    "summary_index"
                } else {
                    "content_index"
                }]
                .as_u64()
                .ok_or(())?;
                let done = kind.ends_with(".done");
                let text = v[if done { "text" } else { "delta" }].as_str().ok_or(())?;
                self.text(index, part, reasoning, text, done, &mut out)?;
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
                    "length"
                } else if self.next_slot > 0 {
                    "tool_calls"
                } else {
                    "stop"
                };
                if let Some(usage) = response.get("usage").filter(|u| !u.is_null()) {
                    self.usage = Some(Usage::from_value(usage).ok_or(())?);
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
            .filter(|((_, _, r), _)| !r)
            .map(|(_, v)| v.as_str())
            .collect();
        let reasoning: String = self
            .fields
            .iter()
            .filter(|((_, _, r), _)| *r)
            .map(|(_, v)| v.as_str())
            .collect();
        let mut assistant =
            json!({"role":"assistant", "content":if text.is_empty() { Value::Null } else { json!(text) }});
        if !reasoning.is_empty() {
            assistant["reasoning_content"] = json!(reasoning);
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
    fn eof_without_terminal_is_failed() {
        assert!(Stream::new("test-model", 7, false).finish().failed);
    }
}
