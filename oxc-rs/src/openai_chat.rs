//! OpenAI Chat Completions request filtering, passthrough shape checks and usage.
use crate::openai_contract::{effort, output_limit, single_choice, upstream, validate, Error};
use serde_json::{json, Value};

/// Only reviewed exact model IDs have a Chat Completions tool contract.
pub fn request(payload: &Value, model: &str, maximum: Option<u64>) -> Result<Value, Error> {
    validate(payload)?;
    let reasoning = match model {
        "gpt-4.1"
        | "gpt-4.1-2025-04-14"
        | "gpt-4.1-mini"
        | "gpt-4.1-mini-2025-04-14"
        | "gpt-4.1-nano"
        | "gpt-4.1-nano-2025-04-14" => false,
        "o3" | "o3-2025-04-16" | "o4-mini" | "o4-mini-2025-04-16" => true,
        _ => {
            return Err(Error {
                status: 400,
                code: "unsupported_model",
                param: "model",
            })
        }
    };
    single_choice(payload)?;
    let mut body = json!({"model":model, "store":false,
        "max_completion_tokens":output_limit(payload, maximum)?});
    for field in [
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
    ] {
        if let Some(value) = payload.get(field) {
            body[field] = value.clone();
        }
    }
    let effort = effort(payload)?;
    if reasoning {
        for field in [
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
            "logit_bias",
            "logprobs",
            "top_logprobs",
            "stop",
            "seed",
        ] {
            body.as_object_mut().unwrap().remove(field);
        }
        if let Some(effort) = effort {
            body["reasoning_effort"] = json!(match effort {
                "none" | "minimal" => "low",
                "xhigh" => "high",
                _ => effort,
            });
        }
    }
    if body["stream"] == true {
        body["stream_options"] = json!({"include_usage":true});
    }
    Ok(body)
}

/// Cache tokens are included in prompt_tokens. No price calculation occurs here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

impl Usage {
    pub fn from_response(value: &Value) -> Option<Self> {
        let usage = value.get("usage")?.as_object()?;
        let prompt = usage.get("prompt_tokens")?.as_u64()?;
        let completion = usage.get("completion_tokens")?.as_u64()?;
        prompt.checked_add(completion)?;
        let details = |field: &str| -> Option<Value> {
            match usage.get(field) {
                None | Some(Value::Null) => Some(json!({})),
                Some(value) if value.is_object() => Some(value.clone()),
                _ => None,
            }
        };
        let prompt_details = details("prompt_tokens_details")?;
        let completion_details = details("completion_tokens_details")?;
        let count = |field: &str, detail: &Value, detail_field: &str| -> Option<u64> {
            detail
                .get(detail_field)
                .or_else(|| usage.get(field))
                .map_or(Some(0), Value::as_u64)
        };
        // Validate both spellings even when the detail object wins precedence.
        for field in ["cache_read_input_tokens", "cache_creation_input_tokens"] {
            if usage.get(field).is_some_and(|v| v.as_u64().is_none()) {
                return None;
            }
        }
        let cached = count("cache_read_input_tokens", &prompt_details, "cached_tokens")?;
        let written = count(
            "cache_creation_input_tokens",
            &prompt_details,
            "cache_write_tokens",
        )?;
        let reasoning = completion_details
            .get("reasoning_tokens")
            .map_or(Some(0), Value::as_u64)?;
        if cached.checked_add(written)? > prompt || reasoning > completion {
            return None;
        }
        Some(Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cache_read_tokens: cached,
            cache_write_tokens: written,
            reasoning_tokens: reasoning,
        })
    }
    pub fn uncached_input_tokens(self) -> u64 {
        self.prompt_tokens
            .saturating_sub(self.cache_read_tokens)
            .saturating_sub(self.cache_write_tokens)
    }
}

fn optional_text(value: &Value, field: &str) -> bool {
    value
        .get(field)
        .is_none_or(|v| v.is_null() || v.is_string())
}

fn message_shape(message: &Value, partial: bool) -> Result<(), Error> {
    if !message.is_object()
        || !["content", "reasoning", "reasoning_content", "refusal"]
            .iter()
            .all(|f| optional_text(message, f))
    {
        return Err(upstream());
    }
    if partial {
        if message.get("role").is_some_and(|r| r != "assistant") {
            return Err(upstream());
        }
    } else if message["role"] != "assistant" {
        return Err(upstream());
    }
    if let Some(calls) = message.get("tool_calls").filter(|v| !v.is_null()) {
        for call in calls.as_array().ok_or_else(upstream)? {
            if !call.is_object()
                || (partial && call["index"].as_u64().is_none())
                || (!partial && !call["id"].is_string())
                || call.get("id").is_some_and(|id| !id.is_string())
                || call.get("type").is_some_and(|kind| kind != "function")
            {
                return Err(upstream());
            }
            let function = &call["function"];
            if !function.is_object()
                || !optional_text(function, "name")
                || !optional_text(function, "arguments")
                || (!partial
                    && (!function["name"].is_string() || !function["arguments"].is_string()))
            {
                return Err(upstream());
            }
        }
    }
    Ok(())
}

fn envelope(value: &Value, stream: bool) -> Result<&Vec<Value>, Error> {
    let object = if stream {
        "chat.completion.chunk"
    } else {
        "chat.completion"
    };
    if value["object"] != object
        || !value["id"].is_string()
        || !value["model"].is_string()
        || value["created"].as_u64().is_none()
    {
        return Err(upstream());
    }
    let choices = value["choices"].as_array().ok_or_else(upstream)?;
    if choices.len() > 1 || (!stream && choices.len() != 1) {
        return Err(upstream());
    }
    if let Some(choice) = choices.first() {
        if choice["index"].as_u64() != Some(0) || !optional_text(choice, "finish_reason") {
            return Err(upstream());
        }
        message_shape(&choice[if stream { "delta" } else { "message" }], stream)?;
    }
    Ok(choices)
}

fn normalize_calls(message: &mut Value) {
    if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
        for (index, call) in calls.iter_mut().enumerate() {
            let call = call.as_object_mut().expect("shape checked");
            call.entry("type").or_insert(json!("function"));
            call.entry("index").or_insert(json!(index));
        }
    }
}

/// Keep valid provider fields and supply the public model/tool defaults. Missing
/// or malformed usage returns None so the caller cannot settle it as actual usage.
pub fn response(value: &Value, public_model: &str) -> Result<(Value, Option<Usage>), Error> {
    let choices = envelope(value, false)?;
    if !choices[0]["finish_reason"].is_string() {
        return Err(upstream());
    }
    let mut out = value.clone();
    out["model"] = json!(public_model);
    normalize_calls(&mut out["choices"][0]["message"]);
    Ok((out, Usage::from_response(value)))
}

pub struct Completed {
    pub usage: Option<Usage>,
    pub finish_reason: String,
}

/// Framing is external: call done() only for the decoded SSE [DONE] marker, and
/// finish() at EOF. Usage frames become trusted only after both terminal signals.
pub struct Stream {
    model: String,
    usage: Option<Usage>,
    finish: Option<String>,
    done: bool,
    failed: bool,
}

impl Stream {
    pub fn new(public_model: impl Into<String>) -> Self {
        Self {
            model: public_model.into(),
            usage: None,
            finish: None,
            done: false,
            failed: false,
        }
    }
    pub fn event(&mut self, value: &Value) -> Result<Value, Error> {
        if self.failed || self.done {
            self.failed = true;
            return Err(upstream());
        }
        let result = self.translate(value);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn translate(&mut self, value: &Value) -> Result<Value, Error> {
        let choices = envelope(value, true)?;
        if choices.is_empty() {
            self.usage = self
                .finish
                .as_ref()
                .and_then(|_| Usage::from_response(value));
        } else {
            // A later choice frame makes any earlier usage provisional again.
            self.usage = None;
            self.finish = choices[0]["finish_reason"].as_str().map(str::to_owned);
        }
        let mut out = value.clone();
        out["model"] = json!(self.model);
        if !choices.is_empty() {
            normalize_calls(&mut out["choices"][0]["delta"]);
        }
        Ok(out)
    }
    pub fn done(&mut self) -> Result<(), Error> {
        if self.failed || self.finish.is_none() {
            self.failed = true;
            return Err(upstream());
        }
        self.done = true;
        Ok(())
    }
    pub fn finish(self) -> Result<Completed, Error> {
        if self.failed || !self.done {
            return Err(upstream());
        }
        Ok(Completed {
            usage: self.usage,
            finish_reason: self.finish.ok_or_else(upstream)?,
        })
    }
}
