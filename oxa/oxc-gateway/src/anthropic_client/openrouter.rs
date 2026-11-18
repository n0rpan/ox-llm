//! OpenRouter Chat Completions transport using the gateway's protected key,
//! cancellation, bounded SSE decoder and connection pool. Never forwards provider
//! errors or unrecognized response fields to the caller.
use super::*;
use crate::routing::{ProviderRouting, Sort};
use reqwest::header::AUTHORIZATION;
use serde_json::json;

impl Client {
    pub(crate) async fn stream_openrouter<F>(
        &self,
        chat: Chat<'_>,
        key: &Key,
        cancel: &Cancellation,
        on: &mut F,
        terminal_usage: &mut Option<Usage>,
        terminal_reported_cost: &mut Option<f64>,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        // Reuse the same HTTPS/loopback-only and no-userinfo/query validation.
        let mut endpoint = messages_endpoint(chat.base_url)?;
        let path = format!(
            "{}/chat/completions",
            endpoint.path().strip_suffix("/messages").ok_or(Error::Endpoint)?
        );
        endpoint.set_path(&path);
        let body = request(&chat)?;
        let body = serde_json::to_vec(&body).map_err(|_| Error::Protocol)?;
        if body.len() > MAX_BODY {
            return Err(Error::TooLarge);
        }
        let mut credential =
            HeaderValue::from_str(&format!("Bearer {}", key.expose())).map_err(|_| Error::Setup)?;
        credential.set_sensitive(true);
        let request = self
            .0
            .post(endpoint)
            .header(AUTHORIZATION, credential)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .body(body)
            .build()
            .map_err(|_| Error::Endpoint)?;
        if cancel.is_cancelled() || on(Event::Sending) == Control::Stop {
            return Err(Error::Cancelled);
        }
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            response = self.0.execute(request) => response.map_err(|_| Error::Transport)?,
        };
        if !response.status().is_success() {
            return Err(Error::Http(response.status().as_u16()));
        }
        if !response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
            })
        {
            return Err(Error::Protocol);
        }
        let mut stream = Stream::new(&chat, key);
        let mut decoder = Framer::default();
        let mut source = response.bytes_stream();
        let mut received = 0_usize;
        loop {
            let bytes = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Error::Cancelled),
                next = source.next() => next.ok_or(Error::Protocol)?.map_err(|_| Error::Transport)?,
            };
            for byte in bytes {
                received += 1;
                if received > MAX_BODY {
                    return Err(Error::TooLarge);
                }
                let Some(data) = decoder.push(byte) else {
                    continue;
                };
                if data == b"[DONE]" {
                    let reported_cost = stream.reported_cost;
                    let (completed, chunks) = stream.finish()?;
                    // Trust metering only after a complete protocol, but save it
                    // before a callback can cancel final delivery or opt out.
                    *terminal_usage = completed.usage;
                    *terminal_reported_cost = reported_cost;
                    deliver(chunks, cancel, on)?;
                    if cancel.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    return Ok(completed);
                }
                let event = serde_json::from_slice(&data).map_err(|_| Error::Protocol)?;
                deliver(stream.event(&event)?, cancel, on)?;
            }
        }
    }
}

fn request(chat: &Chat<'_>) -> Result<Value, Error> {
    let mut body = json!({"model":chat.upstream_model,"stream":true,
        "stream_options":{"include_usage":true}});
    // The last sorting suffix wins. Preserve the configured model's other variants.
    if let Some(suffix) = chat
        .routing
        .and_then(|routing| routing.sort)
        .and_then(Sort::model_suffix)
        .filter(|suffix| !chat.upstream_model.ends_with(*suffix))
    {
        body["model"] = json!(format!("{}{suffix}", chat.upstream_model));
    }
    // O-384: oxa enforces no provider policy. Only the user's own preferences are sent;
    // with none configured the request carries no `provider` object at all.
    if let Some(provider) = chat.routing.and_then(ProviderRouting::to_json) {
        body["provider"] = provider;
    }
    // Local-only template/cache knobs and caller provider overrides never cross
    // the boundary. Tools and history retain the standard Chat Completions shape.
    for field in [
        "messages",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "temperature",
        "top_p",
        "stop",
        "seed",
        "presence_penalty",
        "frequency_penalty",
        "response_format",
    ] {
        if let Some(value) = chat.payload.get(field) {
            body[field] = value.clone();
        }
    }
    let maximum = chat
        .max_output
        .filter(|n| *n > 0)
        .ok_or(Error::Request("model"))?;
    let limit = chat
        .payload
        .get("max_completion_tokens")
        .or_else(|| chat.payload.get("max_tokens"))
        .map(|v| v.as_u64().filter(|n| *n > 0).ok_or(Error::Request("max_tokens")))
        .transpose()?
        .unwrap_or(maximum)
        .min(maximum);
    body["max_tokens"] = json!(limit);
    if chat.payload.get("n").is_some_and(|v| v.as_u64() != Some(1)) {
        return Err(Error::Request("n"));
    }
    let effort = chat
        .payload
        .get("reasoning_effort")
        .filter(|v| !v.is_null())
        .map(|v| v.as_str().ok_or(Error::Request("reasoning_effort")))
        .transpose()?;
    body["reasoning"] = match effort {
        Some("none" | "minimal") => json!({"enabled":false}),
        Some("xhigh") => json!({"effort":"high"}),
        Some(effort @ ("low" | "medium" | "high")) => json!({"effort":effort}),
        Some(_) => return Err(Error::Request("reasoning_effort")),
        None => match chat
            .payload
            .pointer("/chat_template_kwargs/enable_thinking")
            .and_then(Value::as_bool)
        {
            Some(enabled) => json!({"enabled":enabled}),
            None => json!({"effort":"medium"}),
        },
    };
    Ok(body)
}

struct Stream<'a> {
    model: &'a str,
    created: u64,
    include_usage: bool,
    redactor: KeyRedactor<'a>,
    text: String,
    reasoning: String,
    tools: BTreeMap<u64, Value>,
    reason: Option<&'static str>,
    usage: Option<Usage>,
    reasoning_tokens: u64,
    reported_cost: Option<f64>,
}
impl<'a> Stream<'a> {
    fn new(chat: &Chat<'a>, key: &'a Key) -> Self {
        Self {
            model: chat.alias,
            created: chat.created,
            include_usage: chat.payload.pointer("/stream_options/include_usage") == Some(&Value::Bool(true)),
            redactor: KeyRedactor {
                key,
                fields: BTreeMap::new(),
            },
            text: String::new(),
            reasoning: String::new(),
            tools: BTreeMap::new(),
            reason: None,
            usage: None,
            reasoning_tokens: 0,
            reported_cost: None,
        }
    }
    fn chunk(&self, delta: Value, reason: Option<&str>) -> Value {
        json!({"id":"chatcmpl_openrouter","object":"chat.completion.chunk","created":self.created,
            "model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
    }
    fn event(&mut self, event: &Value) -> Result<Vec<Value>, Error> {
        if event.get("error").is_some() {
            return Err(Error::Protocol);
        }
        let choices = event["choices"].as_array().ok_or(Error::Protocol)?;
        let mut out = Vec::new();
        if let Some(raw) = event.get("usage").filter(|v| !v.is_null()) {
            let finishes_here = choices.len() == 1 && choices[0]["finish_reason"].is_string();
            if (self.reason.is_none() && !finishes_here) || self.usage.is_some() {
                return Err(Error::Protocol);
            }
            let usage = usage(raw)?;
            let reasoning_tokens = raw
                .pointer("/completion_tokens_details/reasoning_tokens")
                .map_or(Some(0), Value::as_u64)
                .filter(|tokens| *tokens <= usage.output_tokens)
                .ok_or(Error::Protocol)?;
            self.usage = Some(usage);
            self.reasoning_tokens = reasoning_tokens;
            // Only the upstream terminal usage can supply cost. Missing/invalid
            // values retain catalog pricing; zero is a valid reported charge.
            self.reported_cost = raw["cost"]
                .as_f64()
                .filter(|cost| cost.is_finite() && *cost >= 0.0);
        }
        if choices.is_empty() {
            return Ok(out);
        }
        if choices.len() != 1 || choices[0]["index"].as_u64() != Some(0) || self.reason.is_some() {
            return Err(Error::Protocol);
        }
        let choice = &choices[0];
        let delta = choice["delta"].as_object().ok_or(Error::Protocol)?;
        // Match legacy normalization: the alias is used only when the
        // canonical field is absent, even if it is present but empty/null.
        let reasoning = if delta.contains_key("reasoning_content") {
            "reasoning_content"
        } else {
            "reasoning"
        };
        for (name, field) in [("content", Field::Content), (reasoning, Field::Thinking)] {
            if let Some(value) = delta.get(name).filter(|v| !v.is_null()) {
                let text = value.as_str().ok_or(Error::Protocol)?;
                let safe = self.redactor.feed(field, text);
                if field == Field::Content {
                    self.text.push_str(&safe);
                } else {
                    self.reasoning.push_str(&safe);
                }
                if !safe.is_empty() {
                    let name = if field == Field::Content {
                        "content"
                    } else {
                        "reasoning_content"
                    };
                    out.push(self.chunk(json!({name:safe}), None));
                }
            }
        }
        if let Some(calls) = delta.get("tool_calls").filter(|v| !v.is_null()) {
            for (position, call) in calls.as_array().ok_or(Error::Protocol)?.iter().enumerate() {
                // Legacy normalization supplies array position only for an
                // absent index; an explicitly invalid identity is still refused.
                let index = match call.get("index") {
                    Some(value) => value.as_u64().ok_or(Error::Protocol)?,
                    None => u64::try_from(position).map_err(|_| Error::Protocol)?,
                };
                let tool = self.tools.entry(index).or_insert_with(
                    || json!({"type":"function","id":"","function":{"name":"","arguments":""}}),
                );
                // IDs are atomic snapshots; names and arguments may fragment.
                // Hold all tool fields until completion and decoded-key checks.
                for path in ["/id", "/function/name", "/function/arguments"] {
                    if let Some(value) = call.pointer(path) {
                        // Normalize argument containers before the existing
                        // decoded-object and held-key checks at final delivery.
                        let part = match value {
                            Value::Object(_) | Value::Array(_) if path == "/function/arguments" => {
                                value.to_string()
                            }
                            Value::Null => String::new(),
                            _ => value.as_str().ok_or(Error::Protocol)?.to_owned(),
                        };
                        let target = tool.pointer_mut(path).ok_or(Error::Protocol)?;
                        // Match ReplyAssembler: the latest nonempty ID replaces.
                        if path == "/id" {
                            if !part.is_empty() {
                                *target = json!(part);
                            }
                            continue;
                        }
                        // A repeated whole name is idempotent;
                        // a different fragment still appends, with all fields held.
                        if path == "/function/name" && target.as_str() == Some(part.as_str()) {
                            continue;
                        }
                        *target = json!(format!("{}{part}", target.as_str().ok_or(Error::Protocol)?));
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").filter(|v| !v.is_null()) {
            self.reason = Some(match reason.as_str() {
                Some("stop") => "stop",
                Some("length") => "length",
                Some("tool_calls") => "tool_calls",
                Some("content_filter") => "content_filter",
                _ => return Err(Error::Protocol),
            });
        }
        Ok(out)
    }
    fn finish(mut self) -> Result<(Completed, Vec<Value>), Error> {
        let reason = self.reason.ok_or(Error::Protocol)?;
        let usage = self.usage.ok_or(Error::Protocol)?;
        let mut out = Vec::new();
        let text = self.redactor.finish(Field::Content);
        self.text.push_str(&text);
        if !text.is_empty() {
            out.push(self.chunk(json!({"content":text}), None));
        }
        let reasoning = self.redactor.finish(Field::Thinking);
        self.reasoning.push_str(&reasoning);
        if !reasoning.is_empty() {
            out.push(self.chunk(json!({"reasoning_content":reasoning}), None));
        }
        let mut assistant = json!({"role":"assistant","content":self.text});
        if !self.reasoning.is_empty() {
            assistant["reasoning_content"] = json!(self.reasoning);
        }
        // As in Go, a length stop cannot authorize tool execution. Discard
        // held fragments (even complete JSON) before parsing or publication;
        // trusted terminal usage still settles the request normally.
        if reason == "length" {
            self.tools.clear();
        }
        let mut tools = Vec::new();
        for (slot, tool) in self.tools.values_mut().enumerate() {
            let arguments: Value =
                serde_json::from_str(tool["function"]["arguments"].as_str().ok_or(Error::Protocol)?)
                    .map_err(|_| Error::Protocol)?;
            if tool["id"].as_str().is_none_or(str::is_empty)
                || tool["function"]["name"].as_str().is_none_or(str::is_empty)
                || !arguments.is_object()
            {
                return Err(Error::Protocol);
            }
            // Executable arguments get decoded again by the tool dispatcher.
            // Inspect that tree (including nested values and object keys) with
            // the existing exact held-key redactor before releasing any calls.
            // Refuse rather than rewrite identities or clean raw argument bytes.
            let unsafe_arguments = redaction::contains_secret(&arguments, self.redactor.key.expose());
            let unsafe_tool = redaction::contains_secret(tool, self.redactor.key.expose());
            if unsafe_arguments || unsafe_tool {
                return Err(Error::Protocol);
            }
            tools.push(tool.clone());
            tool["index"] = json!(slot);
        }
        if !tools.is_empty() {
            assistant["tool_calls"] = json!(tools);
            let calls: Vec<_> = self.tools.into_values().collect();
            // Construct before moving self.tools to keep normalized metadata.
            out.push(json!({"id":"chatcmpl_openrouter","object":"chat.completion.chunk","created":self.created,
                "model":self.model,"choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":null}]}));
        }
        out.push(
            json!({"id":"chatcmpl_openrouter","object":"chat.completion.chunk","created":self.created,
            "model":self.model,"choices":[{"index":0,"delta":{},"finish_reason":reason}]}),
        );
        if self.include_usage {
            let mut normalized = usage.openai();
            normalized["completion_tokens_details"]["reasoning_tokens"] = json!(self.reasoning_tokens);
            if let Some(cost) = self.reported_cost {
                normalized["cost"] = json!(cost);
            }
            out.push(
                json!({"id":"chatcmpl_openrouter","object":"chat.completion.chunk","created":self.created,
                "model":self.model,"choices":[],"usage":normalized}),
            );
        }
        Ok((
            Completed {
                assistant,
                native_blocks: Vec::new(),
                usage: Some(usage),
                finish_reason: reason,
            },
            out,
        ))
    }
}
fn usage(raw: &Value) -> Result<Usage, Error> {
    let prompt = raw["prompt_tokens"].as_u64().ok_or(Error::Protocol)?;
    let output = raw["completion_tokens"].as_u64().ok_or(Error::Protocol)?;
    let cached = raw
        .pointer("/prompt_tokens_details/cached_tokens")
        .map_or(Some(0), Value::as_u64)
        .ok_or(Error::Protocol)?;
    let written = raw
        .pointer("/prompt_tokens_details/cache_write_tokens")
        .map_or(Some(0), Value::as_u64)
        .ok_or(Error::Protocol)?;
    prompt.checked_add(output).ok_or(Error::Protocol)?;
    let input = prompt
        .checked_sub(cached)
        .and_then(|n| n.checked_sub(written))
        .ok_or(Error::Protocol)?;
    Ok(Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: written,
    })
}
