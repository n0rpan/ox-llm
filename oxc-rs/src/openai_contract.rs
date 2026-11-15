//! Shared OpenAI input shape contracts; no IO, keys or runtime policy.
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    pub status: u16,
    pub code: &'static str,
    pub param: &'static str,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.code, self.param)
    }
}
impl std::error::Error for Error {}

pub(crate) fn invalid(param: &'static str) -> Error {
    Error {
        status: 400,
        code: "invalid_value",
        param,
    }
}
pub(crate) fn upstream() -> Error {
    Error {
        status: 502,
        code: "cloud_upstream_error",
        param: "model",
    }
}

fn nonempty(value: &Value) -> bool {
    value.as_str().is_some_and(|s| !s.is_empty())
}

/// Check structured inputs before history lookup, reservation or transport.
pub fn validate(payload: &Value) -> Result<(), Error> {
    if !payload.is_object() {
        return Err(invalid("messages"));
    }
    if let Some(messages) = payload.get("messages") {
        for message in messages.as_array().ok_or(invalid("messages"))? {
            let role = message["role"].as_str().ok_or(invalid("messages"))?;
            if !matches!(role, "system" | "developer" | "user" | "assistant" | "tool") {
                return Err(invalid("messages"));
            }
            match &message["content"] {
                Value::Null if role == "assistant" => {}
                Value::String(_) => {}
                Value::Array(parts) => {
                    for part in parts {
                        match part["type"].as_str() {
                            Some("text") if part["text"].is_string() => {}
                            Some("image_url")
                                if role == "user"
                                    && nonempty(&part["image_url"]["url"])
                                    && part["image_url"].get("detail").is_none_or(|d| {
                                        matches!(d.as_str(), Some("auto" | "low" | "high"))
                                    }) => {}
                            _ => return Err(invalid("messages")),
                        }
                    }
                }
                _ => return Err(invalid("messages")),
            }
            if role == "tool" && !nonempty(&message["tool_call_id"]) {
                return Err(invalid("messages"));
            }
            if let Some(calls) = message.get("tool_calls").filter(|v| !v.is_null()) {
                if role != "assistant" {
                    return Err(invalid("messages"));
                }
                for call in calls.as_array().ok_or(invalid("messages"))? {
                    if call["type"] != "function"
                        || !nonempty(&call["id"])
                        || !nonempty(&call["function"]["name"])
                        || !call["function"]["arguments"].is_string()
                    {
                        return Err(invalid("messages"));
                    }
                }
            }
        }
    }
    if let Some(tools) = payload.get("tools").filter(|v| !v.is_null()) {
        for tool in tools.as_array().ok_or(invalid("tools"))? {
            let f = &tool["function"];
            if tool["type"] != "function"
                || !nonempty(&f["name"])
                || f.get("parameters").is_some_and(|p| !p.is_object())
                || f.get("strict")
                    .is_some_and(|s| !s.is_null() && !s.is_boolean())
            {
                return Err(invalid("tools"));
            }
        }
    }
    if let Some(choice) = payload.get("tool_choice").filter(|v| !v.is_null()) {
        if choice.is_object() {
            if choice["type"] != "function" || !nonempty(&choice["function"]["name"]) {
                return Err(invalid("tool_choice"));
            }
        } else if !matches!(choice.as_str(), Some("auto" | "none" | "required")) {
            return Err(invalid("tool_choice"));
        }
    }
    Ok(())
}

pub(crate) fn output_limit(payload: &Value, maximum: Option<u64>) -> Result<u64, Error> {
    let maximum = maximum.filter(|v| *v > 0).ok_or(invalid("model"))?;
    let value = payload
        .get("max_completion_tokens")
        .filter(|v| !v.is_null())
        .or_else(|| payload.get("max_tokens").filter(|v| !v.is_null()));
    value.map_or(Ok(maximum), |v| {
        v.as_u64()
            .filter(|v| *v > 0)
            .map(|v| v.min(maximum))
            .ok_or(invalid("max_completion_tokens"))
    })
}

pub(crate) fn single_choice(payload: &Value) -> Result<(), Error> {
    if payload
        .get("n")
        .is_some_and(|v| !v.is_null() && v.as_u64() != Some(1))
    {
        return Err(invalid("n"));
    }
    Ok(())
}

pub(crate) fn effort(payload: &Value) -> Result<Option<&str>, Error> {
    let effort = match &payload["reasoning_effort"] {
        Value::Null => payload["chat_template_kwargs"]["enable_thinking"]
            .as_bool()
            .map(|v| if v { "high" } else { "none" }),
        Value::String(s) => Some(s.as_str()),
        _ => return Err(invalid("reasoning_effort")),
    };
    if effort
        .is_some_and(|e| !matches!(e, "none" | "minimal" | "low" | "medium" | "high" | "xhigh"))
    {
        return Err(invalid("reasoning_effort"));
    }
    Ok(effort)
}
