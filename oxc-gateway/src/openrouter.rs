//! OpenRouter's OpenAI-compatible protocol, without a network client or retry.
//! Authentication, admission, metering and cancellation belong to the caller.
use crate::{policy::Refusal, redaction, secrets::Key};
use serde_json::{json, Map, Value};

fn invalid(message: &'static str, param: &'static str) -> Refusal {
    Refusal {
        status: 400,
        message,
        code: "invalid_value",
        param,
    }
}
pub fn request(payload: &Value, upstream: &str, max_output: u64) -> Result<Value, Refusal> {
    let payload = payload.as_object().ok_or(Refusal {
        status: 400,
        message: "Invalid cloud request.",
        code: "invalid_request",
        param: "model",
    })?;
    let mut body = Map::new();
    for name in [
        "messages",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "stream",
        "response_format",
        "temperature",
        "top_p",
        "frequency_penalty",
        "presence_penalty",
        "logit_bias",
        "logprobs",
        "top_logprobs",
        "stop",
        "seed",
        "reasoning_effort",
    ] {
        if let Some(value) = payload.get(name) {
            body.insert(name.into(), value.clone());
        }
    }
    if body.get("tools").is_some_and(Value::is_null) {
        body.remove("tools");
    }
    if let Some(tools) = body.get("tools") {
        if tools.as_array().is_none_or(|tools| {
            tools.iter().any(|tool| {
                tool.as_object().is_none_or(|tool| {
                    tool.get("type") != Some(&json!("function"))
                        || !tool.get("function").is_some_and(Value::is_object)
                        || tool
                            .keys()
                            .any(|key| !["type", "function"].contains(&key.as_str()))
                })
            })
        }) {
            return Err(invalid(
                "Only client function tools are supported.",
                "tools",
            ));
        }
    }
    let maximum = if max_output == 0 { 4096 } else { max_output };
    let mut output = maximum;
    for field in ["max_tokens", "max_completion_tokens", "n_predict"] {
        if let Some(value) = payload.get(field) {
            if !integer(value) || value.to_string().starts_with('-') || value == &json!(0) {
                return Err(invalid(
                    "The output token limit must be a positive integer.",
                    field,
                ));
            }
            output = value.as_u64().unwrap_or(maximum).min(maximum);
            break;
        }
    }
    let field = if payload.contains_key("max_completion_tokens") {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    body.insert(field.into(), json!(output));
    body.insert("model".into(), json!(upstream));
    body.insert(
        "provider".into(),
        json!({"zdr":true,"data_collection":"deny"}),
    );
    if let Some(effort) = body
        .get("reasoning_effort")
        .filter(|value| !value.is_null())
    {
        let effort = effort
            .as_str()
            .ok_or_else(|| invalid("\"reasoning_effort\" must be a string.", "reasoning_effort"))?;
        let mapped = match effort {
            "none" | "minimal" => Some("none"),
            "low" => Some("low"),
            "medium" => Some("medium"),
            "high" | "xhigh" => Some("high"),
            _ => None,
        };
        if let Some(mapped) = mapped {
            body.insert("reasoning_effort".into(), json!(mapped));
        } else {
            body.remove("reasoning_effort");
        }
    } else if let Some(enabled) = payload
        .get("chat_template_kwargs")
        .and_then(|v| v.get("enable_thinking"))
        .and_then(Value::as_bool)
    {
        body.insert(
            "reasoning_effort".into(),
            json!(if enabled { "high" } else { "none" }),
        );
    }
    if body.get("stream") == Some(&Value::Bool(true)) {
        body.insert("stream_options".into(), json!({"include_usage":true}));
    }
    Ok(Value::Object(body))
}

fn integer(value: &Value) -> bool {
    value
        .as_number()
        .is_some_and(|n| !n.to_string().contains(['.', 'e', 'E']))
}
fn details(usage: &mut Value, reasoning: i64, timings: &Value) {
    let mut prompt = usage["prompt_tokens_details"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if !prompt.get("cached_tokens").is_some_and(integer) {
        prompt.insert(
            "cached_tokens".into(),
            timings
                .get("cache_n")
                .filter(|value| integer(value) && value.as_f64().is_some_and(|n| n >= 0.0))
                .cloned()
                .unwrap_or(json!(0)),
        );
    }
    let mut completion = usage["completion_tokens_details"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if !completion.get("reasoning_tokens").is_some_and(integer) {
        let count = usage["completion_tokens"]
            .as_i64()
            .map_or(reasoning, |total| reasoning.min(total));
        completion.insert("reasoning_tokens".into(), json!(count));
    }
    usage["prompt_tokens_details"] = json!(prompt);
    usage["completion_tokens_details"] = json!(completion);
}
fn python_json(value: &Value) -> String {
    match value {
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{}: {}", json!(key), python_json(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => value.to_string(),
    }
}
fn calls(value: &mut Value) {
    let Some(calls) = value.as_array_mut() else {
        return;
    };
    for (index, call) in calls.iter_mut().enumerate() {
        let Some(call) = call.as_object_mut() else {
            continue;
        };
        call.entry("type").or_insert(json!("function"));
        call.entry("index").or_insert(json!(index));
        if let Some(function) = call.get_mut("function").and_then(Value::as_object_mut) {
            let args = function.entry("arguments").or_insert(Value::Null);
            if args.is_object() || args.is_array() {
                *args = json!(python_json(args));
            } else if args.is_null() {
                *args = json!("");
            }
        }
    }
}
fn message(value: &mut Value) {
    if !value.is_object() {
        return;
    }
    if value.get("reasoning_content").is_none() && value["reasoning"].is_string() {
        value["reasoning_content"] = value["reasoning"].clone();
    }
    if let Some(value) = value.get_mut("tool_calls") {
        calls(value);
    }
}
fn reasoning(message: &Value, total: &Value) -> i64 {
    let Some(total) = total.as_i64().filter(|n| *n > 0) else {
        return 0;
    };
    let length = |value: &Value| value.as_str().map_or(0, |text| text.chars().count());
    let reasoning = length(&message["reasoning_content"]);
    if reasoning == 0 {
        return 0;
    }
    let mut other = length(&message["content"]);
    if let Some(calls) = message["tool_calls"].as_array() {
        for call in calls {
            other += length(&call["function"]["arguments"]) + length(&call["function"]["name"]);
        }
    }
    ((total as f64 * reasoning as f64 / (reasoning + other) as f64).round_ties_even() as i64)
        .min(total)
}
pub fn response(mut value: Value, alias: &str, key: Option<&Key>) -> Value {
    if let Some(key) = key {
        redaction::value(&mut value, key);
    }
    if !value.is_object() {
        return value;
    }
    value["model"] = json!(alias);
    let mut first = Value::Null;
    if let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices {
            if let Some(value) = choice.get_mut("message") {
                message(value);
                if first.is_null() {
                    first = value.clone();
                }
            }
        }
    }
    let count = reasoning(&first, &value["usage"]["completion_tokens"]);
    let timings = value["timings"].clone();
    if let Some(usage) = value.get_mut("usage").filter(|v| v.is_object()) {
        details(usage, count, &timings);
    }
    value
}

#[derive(Default)]
struct Decoder {
    buffer: Vec<u8>,
    data: Vec<Vec<u8>>,
    name: Option<String>,
}
impl Decoder {
    fn dispatch(&mut self) -> Option<(Option<String>, Vec<u8>)> {
        let name = self.name.take();
        if self.data.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.data).join(&b'\n');
        Some((name, data))
    }
    fn line(&mut self, line: &[u8]) -> Option<(Option<String>, Vec<u8>)> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line.starts_with(b":") {
            return None;
        }
        let (field, value) = line
            .iter()
            .position(|byte| *byte == b':')
            .map_or((line, &b""[..]), |colon| {
                (&line[..colon], &line[colon + 1..])
            });
        let value = value.strip_prefix(b" ").unwrap_or(value);
        if field == b"data" {
            self.data.push(value.into());
        } else if field == b"event" {
            self.name = Some(String::from_utf8_lossy(value).into());
        }
        None
    }
    fn feed(&mut self, data: &[u8], final_chunk: bool) -> Vec<(Option<String>, Vec<u8>)> {
        self.buffer.extend_from_slice(data);
        let mut events = Vec::new();
        while let Some(index) = self.buffer.iter().position(|b| b"\r\n".contains(b)) {
            if self.buffer[index] == b'\r' && index + 1 == self.buffer.len() && !final_chunk {
                break;
            }
            let width = if self.buffer[index] == b'\r' && self.buffer.get(index + 1) == Some(&b'\n')
            {
                2
            } else {
                1
            };
            let line = self.buffer[..index].to_vec();
            self.buffer.drain(..index + width);
            if let Some(event) = self.line(&line) {
                events.push(event);
            }
        }
        if final_chunk {
            let tail = std::mem::take(&mut self.buffer);
            if !tail.is_empty() {
                if let Some(event) = self.line(&tail) {
                    events.push(event);
                }
            }
            if let Some(event) = self.dispatch() {
                events.push(event);
            }
        }
        events
    }
}
fn wire(name: Option<&str>, data: &[u8]) -> Vec<u8> {
    let mut out = name
        .filter(|n| !n.is_empty())
        .map_or(Vec::new(), |name| format!("event: {name}\n").into_bytes());
    out.extend_from_slice(b"data: ");
    out.extend_from_slice(data);
    out.extend_from_slice(b"\n\n");
    out
}
struct Field {
    choice: Value,
    name: String,
    slot: Option<Value>,
    redactor: redaction::Stream,
}
pub struct Stream<'a> {
    alias: String,
    key: Option<&'a Key>,
    parser: Decoder,
    fields: Vec<Field>,
    raw: Option<redaction::Stream>,
    reasoning: i64,
}
impl<'a> Stream<'a> {
    pub fn new(alias: &str, key: Option<&'a Key>) -> Self {
        Self {
            alias: alias.into(),
            key,
            parser: Decoder::default(),
            fields: Vec::new(),
            raw: key.map(redaction::Stream::new),
            reasoning: 0,
        }
    }
    fn fragment(&mut self, choice: &Value, name: &str, slot: Option<Value>, text: &str) -> String {
        let index = self
            .fields
            .iter()
            .position(|field| &field.choice == choice && field.name == name && field.slot == slot)
            .unwrap_or_else(|| {
                self.fields.push(Field {
                    choice: choice.clone(),
                    name: name.into(),
                    slot,
                    redactor: redaction::Stream::new(
                        self.key.expect("field redaction needs a key"),
                    ),
                });
                self.fields.len() - 1
            });
        String::from_utf8(self.fields[index].redactor.feed(text.as_bytes()))
            .expect("ASCII key cannot split UTF-8")
    }
    fn tails(&mut self, indices: Option<&[Value]>) -> Vec<u8> {
        let mut out = Vec::new();
        for field in &mut self.fields {
            if indices.is_some_and(|indices| !indices.contains(&field.choice)) {
                continue;
            }
            let tail = field.redactor.finish();
            if tail.is_empty() {
                continue;
            }
            let text = String::from_utf8(tail).expect("ASCII key suffix");
            let mut delta = Map::new();
            if let Some(slot) = &field.slot {
                let mut function = Map::new();
                function.insert(field.name.clone(), json!(text));
                delta.insert(
                    "tool_calls".into(),
                    json!([{"index":slot,"function":function}]),
                );
            } else {
                delta.insert(field.name.clone(), json!(text));
            }
            let value = json!({"model":self.alias,"object":"chat.completion.chunk",
                "choices":[{"index":field.choice,"delta":delta,"finish_reason":null}]});
            out.extend(wire(None, value.to_string().as_bytes()));
        }
        out
    }
    fn event(&mut self, name: Option<&str>, data: &[u8]) -> Vec<u8> {
        if data == b"[DONE]" {
            let mut out = self.tails(None);
            out.extend(wire(name, data));
            return out;
        }
        if data.is_empty() {
            return Vec::new();
        }
        let Ok(mut value) = serde_json::from_str::<Value>(&String::from_utf8_lossy(data)) else {
            return wire(name, data);
        };
        if !value.is_object() {
            return wire(name, data);
        }
        value["model"] = json!(self.alias);
        if let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) {
            for choice in choices {
                let index = choice.get("index").cloned().unwrap_or(json!(0));
                if let Some(delta) = choice.get_mut("delta").filter(|v| v.is_object()) {
                    if delta.get("reasoning_content").is_none() && delta["reasoning"].is_string() {
                        delta["reasoning_content"] = delta["reasoning"].clone();
                    }
                    if delta["reasoning_content"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty())
                    {
                        self.reasoning += 1;
                    }
                    if let Some(calls_value) = delta.get_mut("tool_calls") {
                        calls(calls_value);
                    }
                    if self.key.is_some() {
                        for field in ["content", "reasoning_content", "refusal"] {
                            if let Some(text) = delta[field].as_str() {
                                delta[field] = json!(self.fragment(&index, field, None, text));
                            }
                        }
                        delta.as_object_mut().unwrap().remove("reasoning");
                        if let Some(calls) =
                            delta.get_mut("tool_calls").and_then(Value::as_array_mut)
                        {
                            for call in calls {
                                let slot = call.get("index").cloned().unwrap_or(json!(0));
                                if let Some(function) =
                                    call.get_mut("function").filter(|v| v.is_object())
                                {
                                    if let Some(text) = function["arguments"].as_str() {
                                        function["arguments"] = json!(self.fragment(
                                            &index,
                                            "arguments",
                                            Some(slot),
                                            text
                                        ));
                                    }
                                    // Names are atomic protocol identifiers: redact them in
                                    // place below, never split them into held fragments (#217).
                                }
                            }
                        }
                    }
                }
                if let Some(value) = choice.get_mut("message") {
                    message(value);
                }
            }
        }
        let timings = value["timings"].clone();
        if let Some(usage) = value.get_mut("usage").filter(|v| v.is_object()) {
            details(usage, self.reasoning, &timings);
        }
        if let Some(key) = self.key {
            redaction::value(&mut value, key);
        }
        let finished = value["choices"]
            .as_array()
            .map(|choices| {
                choices
                    .iter()
                    .filter(|choice| choice.get("finish_reason").is_some_and(|v| !v.is_null()))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let indices = finished
            .iter()
            .map(|c| c.get("index").cloned().unwrap_or(json!(0)))
            .collect::<Vec<_>>();
        let tails = if finished.is_empty() {
            Vec::new()
        } else {
            self.tails(Some(&indices))
        };
        if tails.is_empty() {
            return wire(name, value.to_string().as_bytes());
        }
        let mut ending = value.clone();
        ending["choices"] = json!(finished
            .iter()
            .map(
                |c| json!({"index":c.get("index").cloned().unwrap_or(json!(0)),
            "delta":{},"finish_reason":c["finish_reason"]})
            )
            .collect::<Vec<_>>());
        if let Some(choices) = value.get_mut("choices").and_then(Value::as_array_mut) {
            for choice in choices {
                if choice.get("finish_reason").is_some_and(|v| !v.is_null()) {
                    choice["finish_reason"] = Value::Null;
                }
            }
        }
        value.as_object_mut().unwrap().remove("usage");
        let mut out = wire(name, value.to_string().as_bytes());
        out.extend(tails);
        out.extend(wire(name, ending.to_string().as_bytes()));
        out
    }
    fn process(&mut self, data: &[u8], final_chunk: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, data) in self.parser.feed(data, final_chunk) {
            out.extend(self.event(name.as_deref(), &data));
        }
        if let Some(raw) = &mut self.raw {
            let mut safe = raw.feed(&out);
            if final_chunk {
                safe.extend(raw.finish());
            }
            safe
        } else {
            out
        }
    }
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.process(chunk, false)
    }
    pub fn finish(&mut self) -> Vec<u8> {
        self.process(&[], true)
    }
}
