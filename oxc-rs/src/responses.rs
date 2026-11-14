//! Stateless Responses API translation over the chat engine.
use crate::{
    paths,
    proxy::{complete_usage, python_repr, BadParam},
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};
fn new_id(prefix: &str) -> String {
    let mut bytes = [0_u8; 12];
    getrandom::fill(&mut bytes).expect("response identifier entropy");
    format!(
        "{prefix}_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}
fn bad(message: impl Into<String>, param: &str) -> BadParam {
    BadParam::new(message, param, "unsupported_value")
}
fn invalid(message: impl Into<String>, param: &str) -> BadParam {
    BadParam::new(message, param, "invalid_value")
}
fn pystr(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        _ => value.to_string(),
    }
}
fn text_of(content: &Value, param: &str) -> Result<String, BadParam> {
    if content.is_null() {
        return Ok(String::new());
    }
    if let Some(text) = content.as_str() {
        return Ok(text.to_owned());
    }
    let parts = content.as_array().ok_or_else(|| {
        invalid(
            "Message \"content\" must be a string or a list of parts.",
            param,
        )
    })?;
    let mut texts = Vec::new();
    for part in parts {
        let kind = part["type"].as_str().unwrap_or("");
        if [
            "input_text",
            "output_text",
            "text",
            "summary_text",
            "reasoning_text",
        ]
        .contains(&kind)
            && part["text"].is_string()
        {
            texts.push(part["text"].as_str().unwrap_or("").to_owned());
        } else if kind == "refusal" && part["refusal"].is_string() {
            texts.push(part["refusal"].as_str().unwrap_or("").to_owned());
        } else if ["input_image", "input_file", "input_audio"].contains(&kind) {
            texts.push(format!(
                "[{} omitted: this local model reads text only]",
                kind.trim_start_matches("input_")
            ));
        } else {
            return Err(invalid(
                format!(
                    "Unsupported content part type {}.",
                    python_repr(&pystr(&part["type"]).chars().take(40).collect::<String>())
                ),
                param,
            ));
        }
    }
    Ok(texts.join("\n"))
}
fn messages(payload: &Value) -> Result<Value, BadParam> {
    let mut out = Vec::new();
    let mut pending = None;
    if let Some(instructions) = payload.get("instructions").filter(|v| !v.is_null()) {
        let instructions = instructions
            .as_str()
            .ok_or_else(|| invalid("\"instructions\" must be a string.", "instructions"))?;
        if !instructions.is_empty() {
            out.push(json!({"role":"system","content":instructions}));
        }
    }
    let input = match &payload["input"] {
        Value::Null => json!([]),
        Value::String(text) => json!([{"role":"user","content":text}]),
        items => items.clone(),
    };
    let items = input
        .as_array()
        .ok_or_else(|| invalid("\"input\" must be a string or a list of items.", "input"))?;
    for (index, item) in items.iter().enumerate() {
        let param = format!("input[{index}]");
        if !item.is_object() {
            return Err(invalid("Each input item must be an object.", &param));
        }
        let kind =
            item.get("type")
                .and_then(Value::as_str)
                .unwrap_or(if item.get("role").is_some() {
                    "message"
                } else {
                    ""
                });
        match kind {
            "message"=>{
                let role=item["role"].as_str().unwrap_or("");if !["user","system","developer","assistant"].contains(&role) {return Err(invalid(format!("Unsupported message role {}.",python_repr(&pystr(&item["role"]).chars().take(20).collect::<String>())),&format!("{param}.role")));}
                let text=text_of(&item["content"],&format!("{param}.content"))?;
                pending=if role=="assistant" {Some(out.len())} else {None};out.push(json!({"role":role,"content":text}));
            },
            "function_call"|"custom_tool_call"=>{
                let (Some(name),Some(call_id))=(item["name"].as_str(),item["call_id"].as_str()) else {return Err(invalid(format!("A {kind} item needs \"name\" and \"call_id\"."),&param));};
                let args=if kind=="function_call" {let args=item.get("arguments").cloned().unwrap_or(json!(""));args.as_str().map(str::to_owned).unwrap_or_else(||paths::python_json(&args))} else {paths::python_json(&json!({"input":item["input"].as_str().unwrap_or("")}))};
                let target=if let Some(target)=pending {target} else {let target=out.len();out.push(json!({"role":"assistant","content":""}));pending=Some(target);target};
                let calls=out[target].as_object_mut().expect("message").entry("tool_calls").or_insert_with(||json!([])).as_array_mut().expect("calls");
                calls.push(json!({"id":call_id,"type":"function","function":{"name":name,"arguments":args}}));
            },
            "function_call_output"|"custom_tool_call_output"=>{
                let Some(call_id)=item["call_id"].as_str() else {return Err(invalid(format!("A {kind} item needs \"call_id\"."),&param));};
                let value=item.get("output").cloned().unwrap_or(json!(""));
                let text=if let Some(text)=value.as_str() {text.to_owned()} else if value.is_array() {text_of(&value,"input")?} else {paths::python_json(&value)};
                out.push(json!({"role":"tool","tool_call_id":call_id,"content":text}));pending=None;
            },
            "reasoning"=>{},
            "item_reference"=>return Err(bad("\"item_reference\" input items need stored responses; this local server is stateless. Send the items themselves.",&param)),
            _=>return Err(bad(format!("Unsupported input item type {}.",python_repr(&pystr(&item["type"]).chars().take(40).collect::<String>())),&param)),
        }
    }
    if !out.iter().any(|m| m["role"] != "system") {
        return Err(invalid(
            "\"input\" must contain at least one item.",
            "input",
        ));
    }
    Ok(json!(out))
}
pub(crate) struct Meta {
    id: String,
    created: i64,
    model: Value,
    custom: HashSet<String>,
    request: Value,
}
pub(crate) fn request(payload: Value) -> Result<(Value, Meta), BadParam> {
    if !payload.is_object() {
        return Err(BadParam {
            message: "The request body must be a JSON object.".into(),
            param: None,
            code: "invalid_value",
        });
    }
    for key in ["previous_response_id", "conversation", "prompt"] {
        if !payload[key].is_null() {
            return Err(bad(format!("\"{key}\" is not supported: this local server is stateless and stores no responses or conversations. Send the whole conversation in \"input\"."),key));
        }
    }
    if payload["background"] == true {
        return Err(bad(
            "\"background\" responses are not supported by this local server.",
            "background",
        ));
    }
    let mut tools = Vec::new();
    let mut custom = HashSet::new();
    if let Some(input_tools) = payload.get("tools").filter(|v| !v.is_null()) {
        let input_tools = input_tools
            .as_array()
            .ok_or_else(|| invalid("\"tools\" must be a list.", "tools"))?;
        for (index, tool) in input_tools.iter().enumerate() {
            let kind = tool["type"].as_str().unwrap_or("");
            let name = tool["name"].as_str();
            if !["function", "custom"].contains(&kind) || name.is_none() {
                return Err(bad(format!("Tool type {} is not supported by this local server (function and custom tools only).",python_repr(&pystr(&tool["type"]).chars().take(40).collect::<String>())),&format!("tools[{index}].type")));
            }
            let name = name.expect("validated name");
            let parameters = if kind == "custom" {
                custom.insert(name.to_owned());
                json!({"type":"object","properties":{"input":{"type":"string","description":"The raw input for the tool."}},"required":["input"]})
            } else {
                tool.get("parameters")
                    .filter(|v| {
                        !v.is_null() && **v != json!({}) && **v != json!([]) && **v != json!(false)
                    })
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object","properties":{}}))
            };
            let mut function = json!({"name":name,"parameters":parameters});
            if tool["description"].is_string() {
                function["description"] = tool["description"].clone();
            }
            tools.push(json!({"type":"function","function":function}));
        }
    }
    let mut chat = json!({"messages":messages(&payload)?});
    if let Some(model) = payload.get("model") {
        chat["model"] = model.clone();
    }
    if !tools.is_empty() {
        chat["tools"] = json!(tools);
        if let Some(choice) = payload.get("tool_choice").filter(|v| !v.is_null()) {
            chat["tool_choice"] =
                if [json!("auto"), json!("none"), json!("required")].contains(choice) {
                    choice.clone()
                } else if [json!("function"), json!("custom")].contains(&choice["type"])
                    && choice["name"].is_string()
                {
                    json!({"type":"function","function":{"name":choice["name"]}})
                } else {
                    json!("auto")
                };
        }
    }
    for key in [
        "temperature",
        "top_p",
        "parallel_tool_calls",
        "prompt_cache_key",
        "seed",
        "stop",
        "top_logprobs",
    ] {
        if !payload[key].is_null() {
            chat[key] = payload[key].clone();
        }
    }
    if let Some(limit) = payload.get("max_output_tokens").filter(|v| !v.is_null()) {
        if !limit.as_i64().is_some_and(|n| n >= 1) {
            return Err(invalid(
                "\"max_output_tokens\" must be a positive integer.",
                "max_output_tokens",
            ));
        }
        chat["max_completion_tokens"] = limit.clone();
    }
    if !payload["reasoning"]["effort"].is_null() {
        chat["reasoning_effort"] = payload["reasoning"]["effort"].clone();
    }
    if let Some(fmt) = payload["text"].get("format").filter(|v| v.is_object()) {
        match fmt["type"].as_str().unwrap_or("text") {
            "text" => {}
            "json_object" => chat["response_format"] = json!({"type":"json_object"}),
            "json_schema" => {
                let mut spec = Map::new();
                for key in ["name", "schema", "strict", "description"] {
                    if let Some(value) = fmt.get(key) {
                        spec.insert(key.into(), value.clone());
                    }
                }
                chat["response_format"] = json!({"type":"json_schema","json_schema":spec});
            }
            _ => {
                return Err(invalid(
                    format!(
                        "Unknown \"text.format\" type {}.",
                        python_repr(&pystr(&fmt["type"]).chars().take(40).collect::<String>())
                    ),
                    "text.format.type",
                ))
            }
        }
    }
    if payload["stream"] == true {
        chat["stream"] = json!(true);
        chat["stream_options"] = json!({"include_usage":true});
    }
    let meta = Meta {
        id: new_id("resp"),
        created: chrono::Utc::now().timestamp(),
        model: payload["model"].clone(),
        custom,
        request: payload,
    };
    Ok((chat, meta))
}
impl Meta {
    fn response(
        &self,
        status: &str,
        output: Value,
        usage: Value,
        incomplete: Value,
        error: Value,
    ) -> Value {
        let req = &self.request;
        let mut value = json!({"id":self.id,"object":"response","created_at":self.created,"status":status,"background":false,"error":error,"incomplete_details":incomplete,
            "instructions":req["instructions"].as_str(),"max_output_tokens":req["max_output_tokens"].as_i64(),"model":self.model.as_str().unwrap_or(""),"output":output,
            "parallel_tool_calls":req["parallel_tool_calls"]!=false,"previous_response_id":null,"reasoning":{"effort":req["reasoning"]["effort"],"summary":null},"store":false,
            "temperature":req.get("temperature").cloned().unwrap_or(json!(0)),"text":{"format":req["text"].get("format").filter(|v|v.is_object()).cloned().unwrap_or_else(||json!({"type":"text"}))},
            "tool_choice":req.get("tool_choice").filter(|v|!v.is_null()).cloned().unwrap_or(json!("auto")),"tools":req.get("tools").filter(|v|v.is_array()).cloned().unwrap_or_else(||json!([])),
            "top_p":req.get("top_p").cloned().unwrap_or(json!(1)),"truncation":"disabled","usage":usage,"metadata":req.get("metadata").filter(|v|v.is_object()).cloned().unwrap_or_else(||json!({}))});
        if status == "completed" {
            value["completed_at"] = json!(chrono::Utc::now().timestamp());
        }
        value
    }
    fn call_item(&self, id: &str, call_id: &str, name: &str, args: &str, status: &str) -> Value {
        if self.custom.contains(name) {
            let input = serde_json::from_str::<Value>(if args.is_empty() { "{}" } else { args })
                .ok()
                .and_then(|value| value["input"].as_str().map(str::to_owned))
                .unwrap_or_else(|| args.to_owned());
            json!({"id":id,"type":"custom_tool_call","status":status,"call_id":call_id,"name":name,"input":input})
        } else {
            json!({"id":id,"type":"function_call","status":status,"call_id":call_id,"name":name,"arguments":args})
        }
    }
    pub(crate) fn body(mut self, chat: Value) -> Value {
        if !chat["choices"].is_array() {
            return chat;
        }
        let choice = chat["choices"]
            .as_array()
            .and_then(|v| v.first())
            .cloned()
            .unwrap_or_else(|| json!({}));
        let message = &choice["message"];
        let mut output = Vec::new();
        if let Some(reasoning) = message["reasoning_content"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            output.push(reasoning_item(&new_id("rs"), reasoning, "completed"));
        }
        let calls = message["tool_calls"]
            .as_array()
            .map(|calls| {
                calls
                    .iter()
                    .filter(|c| c.is_object() && c["function"].is_object())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if message["content"].as_str().is_some_and(|s| !s.is_empty()) || calls.is_empty() {
            output.push(message_item(
                &new_id("msg"),
                Some(message["content"].as_str().unwrap_or("")),
                "completed",
            ));
        }
        for call in calls {
            output.push(
                self.call_item(
                    &new_id("fc"),
                    &call["id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| new_id("call")),
                    call["function"]["name"].as_str().unwrap_or(""),
                    call["function"]["arguments"].as_str().unwrap_or(""),
                    "completed",
                ),
            );
        }
        if chat["model"].as_str().is_some_and(|s| !s.is_empty()) {
            self.model = chat["model"].clone();
        }
        let (status, incomplete) = status(&choice["finish_reason"]);
        self.response(
            status,
            json!(output),
            usage(&chat["usage"], &chat["timings"], 0),
            incomplete,
            Value::Null,
        )
    }
}
fn usage(chat: &Value, timings: &Value, reasoning: i64) -> Value {
    let mut value =
        json!({"usage":if chat.is_object() {chat.clone()} else {json!({})},"timings":timings});
    complete_usage(&mut value, reasoning);
    let filled = &value["usage"];
    let prompt = filled["prompt_tokens"].as_i64().unwrap_or(0);
    let done = filled["completion_tokens"].as_i64().unwrap_or(0);
    json!({"input_tokens":prompt,"input_tokens_details":{"cached_tokens":filled["prompt_tokens_details"]["cached_tokens"]},"output_tokens":done,"output_tokens_details":{"reasoning_tokens":filled["completion_tokens_details"]["reasoning_tokens"]},"total_tokens":prompt+done})
}
fn status(finish: &Value) -> (&'static str, Value) {
    if finish == "length" {
        ("incomplete", json!({"reason":"max_output_tokens"}))
    } else {
        ("completed", Value::Null)
    }
}
fn reasoning_item(id: &str, text: &str, status: &str) -> Value {
    json!({"id":id,"type":"reasoning","summary":[],"status":status,"content":[{"type":"reasoning_text","text":text}]})
}
fn content_part(text: &str) -> Value {
    json!({"type":"output_text","text":text,"annotations":[],"logprobs":[]})
}
fn message_item(id: &str, text: Option<&str>, status: &str) -> Value {
    json!({"id":id,"type":"message","status":status,"role":"assistant","content":text.map(|text|vec![content_part(text)]).unwrap_or_default()})
}
struct Item {
    kind: &'static str,
    id: String,
    index: usize,
    text: String,
}
struct Call {
    id: String,
    call_id: String,
    name: String,
    arguments: String,
    index: usize,
}
pub(crate) struct Stream {
    meta: Meta,
    seq: u64,
    started: bool,
    closed: bool,
    done: BTreeMap<usize, Value>,
    next_index: usize,
    current: Option<Item>,
    calls: BTreeMap<i64, Call>,
    call_order: Vec<i64>,
    finish_reason: Value,
    usage: Value,
    timings: Value,
    reasoning: i64,
}
impl Stream {
    pub(crate) fn new(meta: Meta) -> Self {
        Self {
            meta,
            seq: 0,
            started: false,
            closed: false,
            done: BTreeMap::new(),
            next_index: 0,
            current: None,
            calls: BTreeMap::new(),
            call_order: Vec::new(),
            finish_reason: Value::Null,
            usage: Value::Null,
            timings: Value::Null,
            reasoning: 0,
        }
    }
    fn emit(&mut self, kind: &str, fields: Value) -> Vec<u8> {
        let mut value = json!({"type":kind,"sequence_number":self.seq});
        self.seq += 1;
        for (key, value_field) in fields.as_object().expect("event fields") {
            value[key] = value_field.clone();
        }
        format!("event: {kind}\ndata: {}\n\n", paths::python_json(&value)).into_bytes()
    }
    fn start(&mut self) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let response = self.meta.response(
            "in_progress",
            json!([]),
            Value::Null,
            Value::Null,
            Value::Null,
        );
        let mut output = self.emit("response.created", json!({"response":response}));
        output.extend(self.emit("response.in_progress", json!({"response":response})));
        output
    }
    fn allocate(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        index
    }
    fn output(&self) -> Value {
        json!(self.done.values().collect::<Vec<_>>())
    }
    fn close_current(&mut self, status: &str) -> Vec<u8> {
        let Some(item) = self.current.take() else {
            return Vec::new();
        };
        let mut output = Vec::new();
        let common =
            json!({"item_id":item.id,"output_index":item.index,"content_index":0,"text":item.text});
        let done = if item.kind == "reasoning" {
            output.extend(self.emit("response.reasoning_text.done", common));
            reasoning_item(&item.id, &item.text, status)
        } else {
            let mut fields = common;
            fields["logprobs"] = json!([]);
            output.extend(self.emit("response.output_text.done", fields));
            output.extend(self.emit("response.content_part.done",json!({"item_id":item.id,"output_index":item.index,"content_index":0,"part":content_part(&item.text)})));
            message_item(&item.id, Some(&item.text), status)
        };
        self.done.insert(item.index, done.clone());
        output.extend(self.emit(
            "response.output_item.done",
            json!({"output_index":item.index,"item":done}),
        ));
        output
    }
    fn open(&mut self, kind: &'static str) -> Vec<u8> {
        let mut output = self.close_current("completed");
        let index = self.allocate();
        let id = new_id(if kind == "reasoning" { "rs" } else { "msg" });
        let item = if kind == "reasoning" {
            reasoning_item(&id, "", "in_progress")
        } else {
            message_item(&id, None, "in_progress")
        };
        output.extend(self.emit(
            "response.output_item.added",
            json!({"output_index":index,"item":item}),
        ));
        if kind != "reasoning" {
            output.extend(self.emit("response.content_part.added",json!({"item_id":id,"output_index":index,"content_index":0,"part":content_part("")})));
        }
        self.current = Some(Item {
            kind,
            id,
            index,
            text: String::new(),
        });
        output
    }
    fn text(&mut self, kind: &'static str, text: &str) -> Vec<u8> {
        let mut output = if self.current.as_ref().is_none_or(|item| item.kind != kind) {
            self.open(kind)
        } else {
            Vec::new()
        };
        let item = self.current.as_mut().expect("open item");
        item.text.push_str(text);
        let mut fields =
            json!({"item_id":item.id,"output_index":item.index,"content_index":0,"delta":text});
        if kind == "reasoning" {
            self.reasoning += 1;
            output.extend(self.emit("response.reasoning_text.delta", fields));
        } else {
            fields["logprobs"] = json!([]);
            output.extend(self.emit("response.output_text.delta", fields));
        }
        output
    }
    fn tool_delta(&mut self, delta: &Value) -> Vec<u8> {
        let key = delta["index"].as_i64().unwrap_or(0);
        let function = &delta["function"];
        let mut output = Vec::new();
        if !self.calls.contains_key(&key) {
            output.extend(self.close_current("completed"));
            let call = Call {
                id: new_id("fc"),
                call_id: delta["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| new_id("call")),
                name: function["name"].as_str().unwrap_or("").to_owned(),
                arguments: String::new(),
                index: self.allocate(),
            };
            output.extend(self.emit("response.output_item.added",json!({"output_index":call.index,"item":self.meta.call_item(&call.id,&call.call_id,&call.name,"","in_progress")})));
            self.calls.insert(key, call);
            self.call_order.push(key);
        }
        let call = self.calls.get_mut(&key).expect("known call");
        if call.name.is_empty() {
            call.name = function["name"].as_str().unwrap_or("").to_owned();
        }
        if let Some(piece) = function["arguments"].as_str().filter(|s| !s.is_empty()) {
            call.arguments.push_str(piece);
            if !self.meta.custom.contains(&call.name) {
                let fields = json!({"item_id":call.id,"output_index":call.index,"delta":piece});
                output.extend(self.emit("response.function_call_arguments.delta", fields));
            }
        }
        output
    }
    fn close_calls(&mut self, status: &str) -> Vec<u8> {
        let mut output = Vec::new();
        for key in std::mem::take(&mut self.call_order) {
            let call = self.calls.get(&key).expect("known call");
            let id = call.id.clone();
            let index = call.index;
            let name = call.name.clone();
            let args = call.arguments.clone();
            let item = self
                .meta
                .call_item(&id, &call.call_id, &name, &args, status);
            if item["type"] == "custom_tool_call" {
                if item["input"].as_str().is_some_and(|s| !s.is_empty()) {
                    output.extend(self.emit(
                        "response.custom_tool_call_input.delta",
                        json!({"item_id":id,"output_index":index,"delta":item["input"]}),
                    ));
                }
                output.extend(self.emit(
                    "response.custom_tool_call_input.done",
                    json!({"item_id":id,"output_index":index,"input":item["input"]}),
                ));
            } else {
                output.extend(self.emit(
                    "response.function_call_arguments.done",
                    json!({"item_id":id,"output_index":index,"name":name,"arguments":args}),
                ));
            }
            output.extend(self.emit(
                "response.output_item.done",
                json!({"output_index":index,"item":item}),
            ));
            self.done.insert(index, item);
        }
        output
    }
    fn complete(&mut self) -> Vec<u8> {
        if self.closed {
            return Vec::new();
        }
        self.closed = true;
        let mut output = self.start();
        if self.done.is_empty() && self.current.is_none() && self.call_order.is_empty() {
            output.extend(self.open("message"));
        }
        output.extend(self.close_current("completed"));
        output.extend(self.close_calls("completed"));
        let (status, incomplete) = status(&self.finish_reason);
        let used = if self.usage.is_null() {
            Value::Null
        } else {
            usage(&self.usage, &self.timings, self.reasoning)
        };
        let response = self
            .meta
            .response(status, self.output(), used, incomplete, Value::Null);
        output.extend(self.emit(
            if status == "completed" {
                "response.completed"
            } else {
                "response.incomplete"
            },
            json!({"response":response}),
        ));
        output
    }
    fn fail(&mut self, error: &Value) -> Vec<u8> {
        if self.closed {
            return Vec::new();
        }
        self.closed = true;
        let mut output = self.start();
        output.extend(self.close_current("incomplete"));
        output.extend(self.close_calls("incomplete"));
        let code = error["code"].as_str().unwrap_or("server_error");
        let message = if error.is_string() {
            error.as_str().unwrap_or("")
        } else {
            error["message"].as_str().unwrap_or("Local backend error.")
        };
        output.extend(self.emit(
            "error",
            json!({"code":code,"message":message,"param":error["param"]}),
        ));
        let response = self.meta.response(
            "failed",
            self.output(),
            Value::Null,
            Value::Null,
            json!({"code":code,"message":message}),
        );
        output.extend(self.emit("response.failed", json!({"response":response})));
        output
    }
    pub(crate) fn event(&mut self, raw: &[u8]) -> Vec<u8> {
        if raw.starts_with(b":") {
            return [raw, b"\n\n"].concat();
        }
        let lines = raw.split(|b| *b == b'\n').collect::<Vec<_>>();
        let data = lines
            .iter()
            .filter_map(|line| line.strip_prefix(b"data:"))
            .flat_map(|line| line.trim_ascii())
            .copied()
            .collect::<Vec<_>>();
        if data == b"[DONE]" {
            return self.complete();
        }
        let Ok(value) = serde_json::from_slice::<Value>(&data) else {
            return Vec::new();
        };
        if !value.is_object() {
            return Vec::new();
        }
        if value["error"].is_object() || value["error"].is_string() {
            return self.fail(&value["error"]);
        }
        if lines
            .iter()
            .any(|line| line.trim_ascii() == b"event: error")
        {
            return self.fail(&value);
        }
        let mut output = self.start();
        if self.meta.model.as_str().is_none_or(|s| s.is_empty())
            && value["model"].as_str().is_some_and(|s| !s.is_empty())
        {
            self.meta.model = value["model"].clone();
        }
        if value["usage"].is_object() {
            self.usage = value["usage"].clone();
        }
        if value["timings"].is_object() {
            self.timings = value["timings"].clone();
        }
        for choice in value["choices"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|v| v.is_object())
        {
            let delta = &choice["delta"];
            for (kind, key) in [("reasoning", "reasoning_content"), ("message", "content")] {
                if let Some(text) = delta[key].as_str().filter(|s| !s.is_empty()) {
                    output.extend(self.text(kind, text));
                }
            }
            for call in delta["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| v.is_object())
            {
                output.extend(self.tool_delta(call));
            }
            if !choice["finish_reason"].is_null() {
                self.finish_reason = choice["finish_reason"].clone();
            }
        }
        output
    }
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        if !self.closed && !self.finish_reason.is_null() {
            self.complete()
        } else {
            Vec::new()
        }
    }
}
