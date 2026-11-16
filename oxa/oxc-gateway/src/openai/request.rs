//! Pure Chat Completions -> Responses request translation.
use serde_json::{json, Value};

/// The JSON chat request shape, matching the gateway's Anthropic translator.
pub type ChatRequest = Value;

/// A fixed error identifying an invalid field without reflecting request content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestError {
    pub param: &'static str,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Invalid OpenAI request field: {}", self.param)
    }
}
impl std::error::Error for RequestError {}

fn invalid(param: &'static str) -> RequestError {
    RequestError { param }
}

fn array<'a>(value: &'a Value, field: &'static str) -> Result<&'a [Value], RequestError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        _ => Err(invalid(field)),
    }
}

fn nonempty<'a>(value: &'a Value, field: &'static str) -> Result<&'a str, RequestError> {
    value.as_str().filter(|s| !s.is_empty()).ok_or(invalid(field))
}

fn content(value: &Value, role: &str) -> Result<Option<Value>, RequestError> {
    if value.is_string() {
        return Ok(Some(value.clone()));
    }
    if value.is_null() && role == "assistant" {
        return Ok(None);
    }
    let parts = value.as_array().ok_or(invalid("messages"))?;
    let mut result = Vec::new();
    for part in parts {
        match part["type"].as_str() {
            Some("text") if part["text"].is_string() => result.push(json!({
                "type":if role == "assistant" { "output_text" } else { "input_text" },
                "text":part["text"]})),
            Some("image_url") if role == "user" => {
                let image = &part["image_url"];
                let url = nonempty(&image["url"], "messages")?;
                let detail = image.get("detail").and_then(Value::as_str).unwrap_or("auto");
                if !matches!(detail, "auto" | "low" | "high")
                    || image.get("detail").is_some_and(|v| !v.is_string())
                {
                    return Err(invalid("messages"));
                }
                result.push(json!({"type":"input_image","image_url":url,"detail":detail}));
            }
            _ => return Err(invalid("messages")),
        }
    }
    Ok(Some(json!(result)))
}

/// Translate visible, closed history without a continuation cache.
///
/// As in the legacy Responses contract, tool calls must have all results before
/// a later user/assistant turn. A latest continuation needs opaque reasoning and
/// is rejected rather than guessed. No native reasoning or response IDs are restored.
/// `max_output` must be a positive catalog limit. Client limits are clamped to it.
pub fn translate(payload: &ChatRequest, model: &str, max_output: Option<u64>) -> Result<Value, RequestError> {
    if !payload.is_object() {
        return Err(invalid("request"));
    }
    let maximum = max_output.filter(|n| *n > 0).ok_or(invalid("model"))?;
    let limit = ["max_completion_tokens", "max_tokens"]
        .iter()
        .find_map(|field| payload.get(field).filter(|v| !v.is_null()))
        .map(|value| {
            value
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or(invalid("max_completion_tokens"))
        })
        .transpose()?
        .unwrap_or(maximum)
        .min(maximum);
    if payload
        .get("n")
        .is_some_and(|v| !v.is_null() && v.as_u64() != Some(1))
    {
        return Err(invalid("n"));
    }
    let effort = match payload.get("reasoning_effort").filter(|v| !v.is_null()) {
        Some(value) => value.as_str().ok_or(invalid("reasoning_effort"))?,
        None => match payload["chat_template_kwargs"]["enable_thinking"].as_bool() {
            Some(true) => "high",
            Some(false) => "none",
            None => "medium",
        },
    };
    if !matches!(effort, "none" | "minimal" | "low" | "medium" | "high" | "xhigh") {
        return Err(invalid("reasoning_effort"));
    }
    let supports_none = matches!(model, "gpt-6.1-terra" | "gpt-6-sol" | "gpt-6-luna");
    let effort = if effort == "minimal" || (effort == "none" && !supports_none) {
        "low"
    } else {
        effort
    };
    let messages = array(payload, "messages")?;
    let mut inputs = Vec::new();
    let mut pending = std::collections::BTreeSet::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut needs_later_turn = false;
    for message in messages {
        let role = message["role"].as_str().ok_or(invalid("messages"))?;
        if !matches!(role, "system" | "developer" | "user" | "assistant" | "tool") {
            return Err(invalid("messages"));
        }
        if matches!(role, "user" | "assistant") {
            if !pending.is_empty() {
                return Err(invalid("messages"));
            }
            needs_later_turn = false;
        }
        let calls = array(message, "tool_calls").map_err(|_| invalid("messages"))?;
        if role != "assistant" && !calls.is_empty() {
            return Err(invalid("messages"));
        }
        let translated = content(&message["content"], role)?;
        if role == "tool" {
            let id = nonempty(&message["tool_call_id"], "messages")?;
            if !pending.remove(id) {
                return Err(invalid("messages"));
            }
            let output = if let Some(text) = message["content"].as_str() {
                text.to_owned()
            } else {
                message["content"]
                    .as_array()
                    .ok_or(invalid("messages"))?
                    .iter()
                    .map(|p| p["text"].as_str().unwrap_or(""))
                    .collect()
            };
            inputs.push(json!({"type":"function_call_output","call_id":id,"output":output}));
            continue;
        }
        if let Some(content) = translated {
            inputs.push(json!({"role":role,"content":content}));
        }
        for call in calls {
            if call["type"] != "function" {
                return Err(invalid("messages"));
            }
            let id = nonempty(&call["id"], "messages")?;
            let name = nonempty(&call["function"]["name"], "messages")?;
            let arguments = call["function"]["arguments"]
                .as_str()
                .ok_or(invalid("messages"))?;
            if !seen.insert(id) {
                return Err(invalid("messages"));
            }
            pending.insert(id);
            needs_later_turn = true;
            inputs.push(json!({"type":"function_call","call_id":id,"name":name,
                "arguments":arguments,"status":"completed"}));
        }
    }
    if !pending.is_empty() || needs_later_turn {
        return Err(invalid("messages"));
    }
    let mut body = json!({"model":model,"input":inputs,"store":false,
        "include":["reasoning.encrypted_content"],"reasoning":{"effort":effort},
        "max_output_tokens":limit,"stream":payload["stream"] == true});
    let mut tools = Vec::new();
    for tool in array(payload, "tools")? {
        let function = &tool["function"];
        if tool["type"] != "function" || !function.is_object() {
            return Err(invalid("tools"));
        }
        nonempty(&function["name"], "tools")?;
        if function.get("parameters").is_some_and(|v| !v.is_object())
            || function.get("strict").is_some_and(|v| !v.is_boolean())
        {
            return Err(invalid("tools"));
        }
        let mut translated = json!({"type":"function","strict":false});
        for field in ["name", "description", "parameters", "strict"] {
            if let Some(value) = function.get(field) {
                translated[field] = value.clone();
            }
        }
        tools.push(translated);
    }
    let choice = payload.get("tool_choice").filter(|v| !v.is_null());
    let choice = match choice {
        None => json!("auto"),
        Some(value) if matches!(value.as_str(), Some("auto" | "none" | "required")) => value.clone(),
        Some(value) if value["type"] == "function" => json!({"type":"function",
            "name":nonempty(&value["function"]["name"], "tool_choice")?}),
        _ => return Err(invalid("tool_choice")),
    };
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = choice;
        if let Some(value) = payload.get("parallel_tool_calls") {
            body["parallel_tool_calls"] = value.clone();
        }
    }
    if effort == "none" {
        for field in ["temperature", "top_p", "top_logprobs"] {
            if let Some(value) = payload.get(field) {
                body[field] = value.clone();
            }
        }
    }
    let format = &payload["response_format"];
    match format["type"].as_str() {
        Some("json_schema") => {
            let mut translated = json!({"type":"json_schema"});
            if let Some(fields) = format["json_schema"].as_object() {
                for (name, value) in fields {
                    translated[name] = value.clone();
                }
            }
            body["text"] = json!({"format":translated});
        }
        Some("text" | "json_object") => body["text"] = json!({"format":format}),
        _ => {}
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate_request(payload: Value) -> Value {
        translate(&payload, "gpt-6-sol", Some(1024)).unwrap()
    }

    #[test]
    fn text_system_developer_and_output_cap() {
        let body = translate_request(json!({"messages":[
            {"role":"system","content":"system instruction"},
            {"role":"developer","content":"developer instruction"},
            {"role":"user","content":"question"},
            {"role":"assistant","content":[{"type":"text","text":"answer"}]}
        ],"max_completion_tokens":2048,"max_tokens":10,"stream":true,"store":true}));
        assert_eq!(
            body,
            json!({"model":"gpt-6-sol","input":[
            {"role":"system","content":"system instruction"},
            {"role":"developer","content":"developer instruction"},
            {"role":"user","content":"question"},
            {"role":"assistant","content":[{"type":"output_text","text":"answer"}]}
        ],"max_output_tokens":1024,"stream":true,"store":false,
        "include":["reasoning.encrypted_content"],"reasoning":{"effort":"medium"}})
        );
    }

    fn tool_history() -> Value {
        json!({"messages":[
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"call_a","type":"function","function":{"name":"first","arguments":"{\"x\":1}"}},
                {"id":"call_b","type":"function","function":{"name":"second","arguments":"{}"}}
            ]},
            {"role":"tool","tool_call_id":"call_b","content":[{"type":"text","text":"failed: denied"}],"is_error":true},
            {"role":"tool","tool_call_id":"call_a","content":"success"},
            {"role":"user","content":"next turn"}
        ]})
    }

    #[test]
    fn several_closed_calls_and_failed_result_are_replayed_statelessly() {
        let payload = tool_history();
        let body = translate_request(payload.clone());
        assert_eq!(
            body["input"],
            json!([
                {"type":"function_call","call_id":"call_a","name":"first","arguments":"{\"x\":1}","status":"completed"},
                {"type":"function_call","call_id":"call_b","name":"second","arguments":"{}","status":"completed"},
                {"type":"function_call_output","call_id":"call_b","output":"failed: denied"},
                {"type":"function_call_output","call_id":"call_a","output":"success"},
                {"role":"user","content":"next turn"}
            ])
        );
        assert_eq!(body, translate_request(payload));
    }

    #[test]
    fn refused_call_without_result_is_a_typed_error() {
        let mut payload = tool_history();
        payload["messages"].as_array_mut().unwrap().remove(1);
        assert_eq!(
            translate(&payload, "gpt-6-sol", Some(1024)),
            Err(RequestError { param: "messages" })
        );
    }

    #[test]
    fn latest_tool_continuation_is_not_closed_history() {
        let mut payload = tool_history();
        payload["messages"].as_array_mut().unwrap().pop();
        assert_eq!(
            translate(&payload, "gpt-6-sol", Some(1024)),
            Err(RequestError { param: "messages" })
        );
    }

    #[test]
    fn reasoning_effort_and_sampling() {
        for (requested, expected) in [
            ("none", "none"),
            ("minimal", "low"),
            ("low", "low"),
            ("medium", "medium"),
            ("high", "high"),
            ("xhigh", "xhigh"),
        ] {
            let body = translate_request(json!({"reasoning_effort":requested,"temperature":0.5,"top_p":0.9}));
            assert_eq!(body["reasoning"]["effort"], expected);
            assert_eq!(body.get("temperature").is_some(), requested == "none");
            assert_eq!(body.get("top_p").is_some(), requested == "none");
        }
        let payload = json!({"reasoning_effort":"none"});
        assert_eq!(
            translate(&payload, "gpt-6.1-sol", Some(1024)).unwrap()["reasoning"]["effort"],
            "low"
        );
        assert_eq!(
            translate_request(json!({"chat_template_kwargs":{"enable_thinking":false}}))["reasoning"]
                ["effort"],
            "none"
        );
        assert_eq!(
            translate_request(json!({"chat_template_kwargs":{"enable_thinking":true}}))["reasoning"]
                ["effort"],
            "high"
        );
        assert_eq!(
            translate_request(
                json!({"reasoning_effort":"low","chat_template_kwargs":{"enable_thinking":true}})
            )["reasoning"]["effort"],
            "low"
        );
    }

    #[test]
    fn tools_choice_images_and_format() {
        let body = translate_request(json!({"messages":[{"role":"user","content":[
            {"type":"text","text":"inspect"},{"type":"image_url","image_url":{"url":"https://example.invalid/image","detail":"low"}}
        ]}],"tools":[{"type":"function","function":{"name":"inspect","description":"Inspect input","parameters":{"type":"object"}}}],
        "tool_choice":{"type":"function","function":{"name":"inspect"}},"parallel_tool_calls":false,
        "response_format":{"type":"json_schema","json_schema":{"name":"result","schema":{"type":"object"},"strict":true}}}));
        assert_eq!(
            body["tools"],
            json!([{"type":"function","name":"inspect","description":"Inspect input","parameters":{"type":"object"},"strict":false}])
        );
        assert_eq!(body["tool_choice"], json!({"type":"function","name":"inspect"}));
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(
            body["input"][0]["content"][1],
            json!({"type":"input_image","image_url":"https://example.invalid/image","detail":"low"})
        );
        assert_eq!(
            body["text"]["format"],
            json!({"type":"json_schema","name":"result","schema":{"type":"object"},"strict":true})
        );
    }

    #[test]
    fn rejects_invalid_fields_without_panics() {
        for (payload, param) in [
            (json!(null), "request"),
            (json!({"messages":{}}), "messages"),
            (
                json!({"messages":[{"role":"assistant","tool_calls":[{}]}]}),
                "messages",
            ),
            (
                json!({"messages":[{"role":"user","content":[{"type":"audio"}]}]}),
                "messages",
            ),
            (json!({"tools":[{"type":"not_function"}]}), "tools"),
            (json!({"tool_choice":"invalid"}), "tool_choice"),
            (json!({"reasoning_effort":"invalid"}), "reasoning_effort"),
            (json!({"n":2}), "n"),
            (json!({"max_tokens":0}), "max_completion_tokens"),
        ] {
            assert_eq!(
                translate(&payload, "gpt-6-sol", Some(1024)),
                Err(RequestError { param })
            );
        }
        for limit in [None, Some(0)] {
            assert_eq!(
                translate(&json!({}), "gpt-6-sol", limit),
                Err(RequestError { param: "model" })
            );
        }
    }
}
