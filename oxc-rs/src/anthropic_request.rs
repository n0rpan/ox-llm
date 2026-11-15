//! Pure OpenAI chat -> Anthropic Messages translation. No transport or credentials.
use serde_json::{json, Value};

/// A fixed, safe request error. `param` identifies the rejected input field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestError {
    pub param: &'static str,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid Anthropic request field: {}", self.param)
    }
}

impl std::error::Error for RequestError {}

fn thinking(block: &Value) -> bool {
    matches!(
        block["type"].as_str(),
        Some("thinking" | "redacted_thinking")
    )
}

fn text_parts(content: &Value) -> Vec<Value> {
    if let Some(text) = content.as_str() {
        return vec![json!({"type":"text", "text":text})];
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| match part["type"].as_str() {
            Some("text") if part["text"].is_string() => {
                Some(json!({"type":"text", "text":part["text"]}))
            }
            Some("image_url" | "input_image") => Some(json!({"type":"text",
                "text":"[image omitted: the cloud text model reads text only]"})),
            _ => None,
        })
        .collect()
}

fn tool_use(call: &Value) -> Option<Value> {
    let function = call.get("function")?.as_object()?;
    let mut args = function.get("arguments").cloned().unwrap_or(Value::Null);
    if let Some(raw) = args.as_str() {
        args = serde_json::from_str(raw).unwrap_or_else(|_| json!({"input":raw}));
    }
    if !args.is_object() {
        args = json!({"input":args});
    }
    Some(
        json!({"type":"tool_use", "id":nonempty(&call["id"], "toolu_unknown"),
        "name":nonempty(&call["function"]["name"], "tool"), "input":args}),
    )
}

fn nonempty<'a>(value: &'a Value, fallback: &'a str) -> &'a str {
    value.as_str().filter(|s| !s.is_empty()).unwrap_or(fallback)
}

/// Translate without remembered thinking blocks. For continued thinking/tool turns,
/// use [`translate_with_blocks`] and replay the native blocks from the stream result.
pub fn translate(
    payload: &Value,
    upstream_model: &str,
    max_output: Option<u64>,
) -> Result<Value, RequestError> {
    translate_with_blocks(payload, upstream_model, max_output, |_| None)
}

/// `restore` receives the original message index and may return native assistant
/// blocks from that exact conversation prefix. The gateway owns lookup/isolation;
/// never restore blocks from another model, upstream, credential or history prefix.
/// Explicit `thinking_blocks` and native content arrays are also preserved verbatim.
pub fn translate_with_blocks(
    payload: &Value,
    upstream_model: &str,
    max_output: Option<u64>,
    mut restore: impl FnMut(usize) -> Option<Vec<Value>>,
) -> Result<Value, RequestError> {
    if !payload.is_object() {
        return Err(RequestError { param: "request" });
    }
    let maximum = max_output.filter(|n| *n > 0).unwrap_or(4096);
    let mut limit = maximum;
    for field in ["max_tokens", "max_completion_tokens", "n_predict"] {
        if let Some(value) = payload.get(field) {
            limit = value
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or(RequestError { param: field })?
                .min(maximum);
            break;
        }
    }
    let mut system = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    for (index, message) in payload["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        if !message.is_object() {
            continue;
        }
        let role = message["role"].as_str().unwrap_or("");
        if matches!(role, "system" | "developer") {
            let text: String = text_parts(&message["content"])
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect();
            if !text.is_empty() {
                system.push(text);
            }
            continue;
        }
        let blocks = if role == "tool" {
            // The Python reference stringifies arrays and sends empty text for empty
            // results. Preserve text parts and use a nonempty placeholder instead (#334).
            let mut parts = text_parts(&message["content"]);
            parts.retain(|part| part["text"].as_str().is_some_and(|s| !s.is_empty()));
            if parts.is_empty() {
                parts.push(json!({"type":"text", "text":"[empty tool result]"}));
            }
            vec![
                json!({"type":"tool_result", "tool_use_id":nonempty(&message["tool_call_id"], ""),
                "content":parts}),
            ]
        } else if role == "assistant" {
            let native = message["content"]
                .as_array()
                .filter(|parts| parts.iter().any(thinking));
            if let Some(blocks) = restore(index) {
                blocks
            } else if let Some(native) = native {
                native.iter().filter(|b| b.is_object()).cloned().collect()
            } else {
                let mut blocks: Vec<_> = message["thinking_blocks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| thinking(b))
                    .cloned()
                    .collect();
                blocks.extend(text_parts(&message["content"]));
                blocks.extend(
                    message["tool_calls"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(tool_use),
                );
                blocks
            }
        } else {
            text_parts(&message["content"])
        };
        if blocks.is_empty() {
            continue;
        }
        let role = if role == "assistant" {
            "assistant"
        } else {
            "user"
        };
        if let Some(previous) = messages.last_mut().filter(|m| m["role"] == role) {
            previous["content"].as_array_mut().unwrap().extend(blocks);
        } else {
            messages.push(json!({"role":role, "content":blocks}));
        }
    }
    if messages.is_empty() {
        messages.push(json!({"role":"user", "content":[{"type":"text", "text":""}]}));
    }
    let last = messages.last_mut().unwrap()["content"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap();
    if !thinking(last) {
        last["cache_control"] = json!({"type":"ephemeral"});
    }
    let mut body = json!({"model":upstream_model, "messages":messages, "max_tokens":limit});
    if !system.is_empty() {
        body["system"] = json!([{"type":"text", "text":system.join("\n\n"),
            "cache_control":{"type":"ephemeral"}}]);
    }
    let mut tools = Vec::new();
    for tool in payload["tools"].as_array().into_iter().flatten() {
        let function = &tool["function"];
        if !function["name"].is_string() {
            continue;
        }
        tools.push(json!({"name":function["name"],
            "description":function["description"].as_str().unwrap_or(""),
            "input_schema":function.get("parameters").filter(|p| p.is_object()).cloned()
                .unwrap_or_else(|| json!({"type":"object", "properties":{}}))}));
    }
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = json!({"type":"ephemeral"});
        let choice = &payload["tool_choice"];
        let forced = choice == "required" || choice["function"].is_object();
        if forced
            && ["-opus-5-5", "-sonnet-5-5"]
                .iter()
                .any(|s| upstream_model.contains(s))
        {
            return Err(RequestError {
                param: "tool_choice",
            });
        }
        if choice != "none" {
            body["tools"] = json!(tools);
            body["tool_choice"] = if choice == "required" {
                json!({"type":"any"})
            } else if choice["function"].is_object() {
                json!({"type":"tool", "name":nonempty(&choice["function"]["name"], "")})
            } else {
                json!({"type":"auto"})
            };
        }
    }
    if let Some(stop) = payload["stop"].as_str() {
        body["stop_sequences"] = json!([stop]);
    } else if let Some(stops) = payload["stop"].as_array() {
        body["stop_sequences"] = stops
            .iter()
            .filter(|s| s.is_string())
            .take(16)
            .cloned()
            .collect();
    }
    let effort = match &payload["reasoning_effort"] {
        Value::Null => payload["chat_template_kwargs"]["enable_thinking"]
            .as_bool()
            .map(|v| if v { "high" } else { "none" }),
        Value::String(s) => Some(s.as_str()),
        _ => {
            return Err(RequestError {
                param: "reasoning_effort",
            })
        }
    };
    if let Some(effort) = effort {
        let mapped = match effort {
            "low" | "medium" | "high" | "xhigh" => Some(effort),
            "none" | "minimal" => Some("low"),
            _ => None,
        };
        if let Some(mapped) = mapped {
            body["thinking"] = json!({"type":"adaptive"});
            body["output_config"] = json!({"effort":mapped});
        }
    }
    if payload["stream"] == true {
        body["stream"] = json!(true);
    }
    Ok(body)
}
