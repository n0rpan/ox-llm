//! Pure Chat Completions -> Responses request translation.
use serde_json::{json, Value};

/// The JSON chat request shape, matching the gateway's Anthropic translator.
pub type ChatRequest = Value;

/// Route selected by the caller. Subscription adaptation belongs to the transport
/// adapter, not this pure Responses request translator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Api,
    Subscription,
}

/// All provider output items for one assistant turn, in their original order.
/// Persist these values with history; this helper owns no continuation state.
#[derive(Clone)]
pub struct ReplayItems(Vec<Value>);

impl ReplayItems {
    pub fn new(items: Vec<Value>) -> Self {
        Self(items)
    }
}

impl std::fmt::Debug for ReplayItems {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReplayItems([REDACTED])")
    }
}

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

/// Translate history without a continuation cache.
///
/// `replay` is indexed by assistant-turn ordinal (not message index). Missing
/// entries, including omitted trailing entries, mean no persisted provider items.
/// A present entry must contain ALL output items from that response, in order;
/// these replace the entire reconstructed assistant output, including text and
/// function calls. The original non-assistant input and matching tool results
/// remain in history. Callers must scope persisted items to the provider/model/
/// account/prefix; the translator neither stores nor retrieves them.
///
/// All calls still require exactly one matching result. A latest tool continuation
/// without replay items is refused rather than guessing opaque reasoning state.
/// Both modes produce the same Responses body; subscription adaptation is separate.
/// `max_output` must be a positive catalog limit. Client limits are clamped to it.
pub fn translate(
    payload: &ChatRequest,
    model: &str,
    max_output: Option<u64>,
    _mode: Mode,
    replay: &[Option<ReplayItems>],
) -> Result<Value, RequestError> {
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
    let mut assistant_turn = 0;
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
        let items = if role == "assistant" {
            let items = replay.get(assistant_turn).and_then(Option::as_ref);
            assistant_turn += 1;
            items
        } else {
            None
        };
        if items.is_none() {
            if let Some(content) = translated {
                inputs.push(json!({"role":role,"content":content}));
            }
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
            needs_later_turn = items.is_none();
            if items.is_none() {
                inputs.push(json!({"type":"function_call","call_id":id,"name":name,
                    "arguments":arguments,"status":"completed"}));
            }
        }
        if let Some(items) = items {
            // Only inspect the call IDs needed to correlate visible tool results.
            // All other provider fields/items, including reasoning, stay opaque.
            let mut replay_calls = std::collections::BTreeSet::new();
            for item in &items.0 {
                if item["type"] == "function_call" {
                    let id = nonempty(&item["call_id"], "replay_items")?;
                    if !replay_calls.insert(id) {
                        return Err(invalid("replay_items"));
                    }
                }
            }
            if replay_calls != pending {
                return Err(invalid("replay_items"));
            }
            inputs.extend(items.0.iter().cloned());
        }
    }
    if replay.len() > assistant_turn {
        return Err(invalid("replay_items"));
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
        translate(&payload, "gpt-6-sol", Some(1024), Mode::Api, &[]).unwrap()
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
            translate(&payload, "gpt-6-sol", Some(1024), Mode::Api, &[]),
            Err(RequestError { param: "messages" })
        );
    }

    #[test]
    fn latest_tool_continuation_is_not_closed_history() {
        let mut payload = tool_history();
        payload["messages"].as_array_mut().unwrap().pop();
        assert_eq!(
            translate(&payload, "gpt-6-sol", Some(1024), Mode::Api, &[]),
            Err(RequestError { param: "messages" })
        );
    }

    fn replay_fixture() -> ReplayItems {
        ReplayItems::new(vec![
            json!({"type":"reasoning","id":"opaque_reasoning","encrypted_content":"opaque_fixture","summary":[]}),
            json!({"type":"function_call","id":"provider_b","call_id":"call_b","name":"second","arguments":"{}","status":"completed"}),
            json!({"type":"message","id":"provider_message","role":"assistant","content":[{"type":"output_text","text":"working"}]}),
            json!({"type":"function_call","id":"provider_a","call_id":"call_a","name":"first","arguments":"{\"x\":1}","status":"completed"}),
        ])
    }

    #[test]
    fn persisted_two_tool_continuation_survives_a_new_process() {
        const CHILD: &str = "OXA_TEST_OPENAI_REPLAY_CHILD";
        let mut payload = tool_history();
        payload["messages"].as_array_mut().unwrap().pop();
        payload["messages"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!({"role":"user","content":"original input"}));
        let items = replay_fixture();
        // The child receives only serialized history and provider output, not any
        // same-process translator state.
        let (payload, items) = match std::env::var(CHILD) {
            Ok(saved) => {
                let saved: Value = serde_json::from_str(&saved).unwrap();
                (
                    saved["payload"].clone(),
                    ReplayItems::new(saved["items"].as_array().unwrap().clone()),
                )
            }
            Err(_) => (payload, items),
        };
        let body = translate(
            &payload,
            "gpt-6-sol",
            Some(1024),
            Mode::Api,
            &[Some(items.clone())],
        )
        .unwrap();
        let bytes = serde_json::to_string(&body).unwrap();
        if std::env::var_os(CHILD).is_some() {
            println!("REPLAY_RESULT={bytes}");
            return;
        }
        let mut expected = vec![json!({"role":"user","content":"original input"})];
        expected.extend(items.0.clone());
        expected.extend([
            json!({"type":"function_call_output","call_id":"call_b","output":"failed: denied"}),
            json!({"type":"function_call_output","call_id":"call_a","output":"success"}),
        ]);
        assert_eq!(body["input"], json!(expected));
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "openai::request::tests::persisted_two_tool_continuation_survives_a_new_process",
                "--nocapture",
            ])
            .env(
                CHILD,
                serde_json::to_string(&json!({"payload":payload,"items":items.0})).unwrap(),
            )
            .output()
            .unwrap();
        assert!(child.status.success());
        let stdout = String::from_utf8(child.stdout).unwrap();
        assert_eq!(
            stdout
                .lines()
                .find_map(|line| line.strip_prefix("REPLAY_RESULT=")),
            Some(bytes.as_str())
        );
        assert_eq!(
            body,
            translate(
                &payload,
                "gpt-6-sol",
                Some(1024),
                Mode::Subscription,
                &[Some(items)]
            )
            .unwrap()
        );
    }

    #[test]
    fn replay_is_per_assistant_turn_and_replaces_visible_output() {
        let mut payload = tool_history();
        let messages = payload["messages"].as_array_mut().unwrap();
        messages.pop();
        messages.splice(
            0..0,
            [
                json!({"role":"user","content":"first input"}),
                json!({"role":"assistant","content":"visible answer"}),
                json!({"role":"user","content":"second input"}),
            ],
        );
        messages[3]["content"] = json!("visible working text");
        let first = ReplayItems::new(vec![
            json!({"type":"reasoning","encrypted_content":"earlier_opaque_fixture"}),
            json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"provider answer"}]}),
        ]);
        let second = replay_fixture();
        let body = translate(
            &payload,
            "gpt-6-sol",
            Some(1024),
            Mode::Api,
            &[Some(first.clone()), Some(second.clone())],
        )
        .unwrap();
        let mut expected = vec![json!({"role":"user","content":"first input"})];
        expected.extend(first.0);
        expected.push(json!({"role":"user","content":"second input"}));
        expected.extend(second.0);
        expected.extend([
            json!({"type":"function_call_output","call_id":"call_b","output":"failed: denied"}),
            json!({"type":"function_call_output","call_id":"call_a","output":"success"}),
        ]);
        assert_eq!(body["input"], json!(expected));
        let body = translate(
            &payload,
            "gpt-6-sol",
            Some(1024),
            Mode::Api,
            &[None, Some(replay_fixture())],
        )
        .unwrap();
        assert_eq!(
            body["input"][1],
            json!({"role":"assistant","content":"visible answer"})
        );
    }

    #[test]
    fn replay_call_ids_must_match_and_errors_do_not_expose_items() {
        let mut payload = tool_history();
        payload["messages"].as_array_mut().unwrap().pop();
        let mut missing = replay_fixture();
        missing.0.pop();
        let mut mismatch = replay_fixture();
        mismatch.0[1]["call_id"] = json!("opaque_fixture");
        let mut duplicate = replay_fixture();
        duplicate.0.push(duplicate.0[1].clone());
        for replay in [
            vec![Some(missing)],
            vec![Some(mismatch)],
            vec![Some(duplicate)],
            vec![Some(ReplayItems::new(vec![]))],
            vec![Some(replay_fixture()), None],
        ] {
            let error = translate(&payload, "gpt-6-sol", Some(1024), Mode::Api, &replay).unwrap_err();
            assert_eq!(
                error,
                RequestError {
                    param: "replay_items"
                }
            );
            assert_eq!(format!("{error:?}"), "RequestError { param: \"replay_items\" }");
            assert_eq!(error.to_string(), "Invalid OpenAI request field: replay_items");
            assert!(!format!("{replay:?}").contains("opaque_fixture"));
        }
    }

    #[test]
    fn replay_keeps_result_validation_and_diagnostics_redacted() {
        let replay = [Some(replay_fixture())];
        assert_eq!(format!("{replay:?}"), "[Some(ReplayItems([REDACTED]))]");
        for fault in ["missing", "mismatch", "duplicate"] {
            let mut payload = tool_history();
            let messages = payload["messages"].as_array_mut().unwrap();
            messages.pop();
            match fault {
                "missing" => {
                    messages.remove(1);
                }
                "mismatch" => {
                    messages[1]["tool_call_id"] = json!("unknown");
                }
                _ => {
                    messages.insert(2, messages[1].clone());
                }
            }
            let error = translate(&payload, "gpt-6-sol", Some(1024), Mode::Api, &replay).unwrap_err();
            assert_eq!(error, RequestError { param: "messages" });
            assert_eq!(format!("{error:?}"), "RequestError { param: \"messages\" }");
            assert_eq!(error.to_string(), "Invalid OpenAI request field: messages");
        }
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
            translate(&payload, "gpt-6.1-sol", Some(1024), Mode::Api, &[]).unwrap()["reasoning"]["effort"],
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
                translate(&payload, "gpt-6-sol", Some(1024), Mode::Api, &[]),
                Err(RequestError { param })
            );
        }
        for limit in [None, Some(0)] {
            assert_eq!(
                translate(&json!({}), "gpt-6-sol", limit, Mode::Api, &[]),
                Err(RequestError { param: "model" })
            );
        }
    }
}
