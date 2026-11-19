//! Chat-completions transport over the gateway's existing hardened HTTP pool,
//! SSE decoder and decoded-field redactor. No admission, credentials or queue.
use super::{Model, Refusal, SessionHint, Usage};
use crate::{
    anthropic_client::{Control, KeyRedactor},
    anthropic_stream::{Completed, Field, Redactor},
    secrets::Key,
};
use futures_util::StreamExt;
use oxa_http::sse::{Frame, Framer};
use oxa_proto::CancelToken;
use reqwest::{
    header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT},
    Client,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

const MAX_BODY: usize = 64 * 1024 * 1024;
const MAX_DISCOVERY: usize = 4 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Endpoint,
    Request,
    Transport,
    Protocol,
    Discovery(super::DiscoveryProblem),
    TooLarge,
    Cancelled,
    Refused(Refusal),
}
pub enum Event {
    Sending,
    Chunk(Value),
}

fn bearer(key: &Key) -> Result<HeaderValue, Error> {
    let mut header =
        HeaderValue::from_str(&format!("Bearer {}", key.expose())).map_err(|_| Error::Request)?;
    header.set_sensitive(true);
    Ok(header)
}
fn echoes_credential(value: &Value, key: &Key) -> bool {
    match value {
        Value::String(text) => text.contains(key.expose()),
        Value::Array(items) => items.iter().any(|item| echoes_credential(item, key)),
        Value::Object(map) => map
            .iter()
            .any(|(name, item)| name.contains(key.expose()) || echoes_credential(item, key)),
        _ => false,
    }
}
async fn bounded(
    mut response: reqwest::Response,
    max: usize,
    cancel: &CancelToken,
) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    loop {
        let next = tokio::select! { biased; _=cancel.cancelled()=>return Err(Error::Cancelled), next=response.chunk()=>next.map_err(|_|Error::Transport)?, };
        let Some(bytes) = next else { return Ok(out) };
        if bytes.len() > max.saturating_sub(out.len()) {
            return Err(Error::TooLarge);
        }
        out.extend_from_slice(&bytes);
    }
}

fn versioned_endpoint(base: &str, resource: &str) -> Result<reqwest::Url, Error> {
    let mut url = super::endpoint(base).map_err(|_| Error::Endpoint)?;
    url.path_segments_mut()
        .map_err(|_| Error::Endpoint)?
        .pop_if_empty()
        .push("v1")
        .push(resource);
    Ok(url)
}

pub(crate) async fn plan_usage(
    client: &Client,
    base: &str,
    key: &Key,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<Option<super::PlanUsage>, Error> {
    const MAX_USAGE: usize = 16 * 1024;
    let url = versioned_endpoint(base, "usage")?;
    let operation = async {
        let request = client
            .get(url)
            .header(USER_AGENT, concat!("oxa/", env!("CARGO_PKG_VERSION")))
            .header(AUTHORIZATION, bearer(key)?);
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            response = request.send() => response.map_err(|_| Error::Transport)?,
        };
        let status = response.status().as_u16();
        if status != 200 {
            let retry = crate::rate_limits::retry_after(response.headers())
                .and_then(|delay| u32::try_from(delay.as_secs()).ok());
            return Err(Error::Refused(
                super::refusal(status, &Value::Null).with_retry_after(retry),
            ));
        }
        let bytes = bounded(response, MAX_USAGE, cancel).await?;
        let body = serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
        Ok(super::plan_usage::parse(&body))
    };
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| Error::Transport)?
}

pub async fn models(
    client: &Client,
    base: &str,
    key: &Key,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<Vec<Model>, Error> {
    models_observed(client, base, key, timeout, cancel, None).await
}
pub(crate) async fn models_observed(
    client: &Client,
    base: &str,
    key: &Key,
    timeout: Duration,
    cancel: &CancelToken,
    diagnostics: Option<&crate::diagnostics::Diagnostics>,
) -> Result<Vec<Model>, Error> {
    let url = versioned_endpoint(base, "models")?;
    let operation = async {
        let request = client
            .get(url)
            .header(USER_AGENT, concat!("oxa/", env!("CARGO_PKG_VERSION")))
            .header(AUTHORIZATION, bearer(key)?);
        let response = tokio::select! { biased; _=cancel.cancelled()=>return Err(Error::Cancelled), response=request.send()=>response.map_err(|_|Error::Transport)?, };
        let status = response.status().as_u16();
        let retry_after = crate::rate_limits::retry_after(response.headers())
            .and_then(|delay| u32::try_from(delay.as_secs()).ok());
        if let Some(diagnostics) = diagnostics {
            diagnostics.http(
                crate::diagnostics::Stage::DiscoveryHttp,
                status,
                response.headers(),
            );
        }
        let bytes = bounded(response, MAX_DISCOVERY, cancel).await?;
        if status != 200 {
            let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if let Some(diagnostics) = diagnostics {
                diagnostics.error_code(&value);
            }
            return Err(Error::Refused(
                super::refusal(status, &value).with_retry_after(retry_after),
            ));
        }
        let mut value: Value =
            serde_json::from_slice(&bytes).map_err(|_| Error::Discovery(super::DiscoveryProblem::Json))?;
        // Discovery becomes public catalog metadata. Reject an echoed secret
        // anywhere in decoded fields, including names and nested hints; never
        // redact/rewrite an upstream routing identifier into another model.
        if echoes_credential(&value, key) {
            return Err(Error::Discovery(super::DiscoveryProblem::CredentialEcho));
        }
        super::derive_canonical_ids(&mut value).map_err(Error::Discovery)?;
        super::models_diagnostic(&value, base).map_err(|(_, problem)| Error::Discovery(problem))
    };
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| Error::Transport)?
}

/// Terminal usage is returned separately so cancellation after its delivery
/// still settles the reported categories through the existing consuming hold.
pub struct Chat<'a> {
    pub model: &'a Model,
    pub alias: &'a str,
    pub payload: &'a Value,
    pub key: &'a Key,
    pub hint: &'a SessionHint,
}
pub async fn stream<F>(
    client: &Client,
    chat: Chat<'_>,
    cancel: &CancelToken,
    on: &mut F,
    terminal_usage: &mut Option<Usage>,
) -> Result<Completed, Error>
where
    F: FnMut(Event) -> Control + ?Sized,
{
    stream_observed(client, chat, cancel, on, terminal_usage, None).await
}
pub(crate) async fn stream_observed<F>(
    client: &Client,
    chat: Chat<'_>,
    cancel: &CancelToken,
    on: &mut F,
    terminal_usage: &mut Option<Usage>,
    diagnostics: Option<&crate::diagnostics::Diagnostics>,
) -> Result<Completed, Error>
where
    F: FnMut(Event) -> Control + ?Sized,
{
    use crate::diagnostics::{StreamCause, TerminalCheck};
    let protocol = |cause| {
        if let Some(diagnostics) = diagnostics {
            diagnostics.stream_cause(cause);
        }
        Error::Protocol
    };
    let terminal = |check| {
        if let Some(diagnostics) = diagnostics {
            diagnostics.terminal_check(check);
        }
        Error::Protocol
    };
    let transport = || {
        if let Some(diagnostics) = diagnostics {
            diagnostics.stream_cause(StreamCause::Transport);
        }
        Error::Transport
    };
    *terminal_usage = None;
    let Chat {
        model,
        alias,
        payload,
        key,
        hint,
    } = chat;
    let endpoint = model.chat_endpoint().map_err(|_| Error::Endpoint)?;
    let body = serde_json::to_vec(&model.request(payload).map_err(|_| Error::Request)?)
        .map_err(|_| Error::Request)?;
    if body.len() > MAX_BODY {
        return Err(Error::TooLarge);
    }
    if cancel.is_cancelled() || on(Event::Sending) == Control::Stop {
        return Err(Error::Cancelled);
    }
    let request = client
        .post(endpoint)
        .headers(hint.headers())
        .header(AUTHORIZATION, bearer(key)?)
        .header(CONTENT_TYPE, "application/json")
        .header("accept", "text/event-stream")
        .body(body);
    let response = tokio::select! { biased; _=cancel.cancelled()=>return Err(Error::Cancelled), response=request.send()=>response.map_err(|_|transport())?, };
    let status = response.status().as_u16();
    let retry_after = crate::rate_limits::retry_after(response.headers())
        .and_then(|delay| u32::try_from(delay.as_secs()).ok());
    if let Some(diagnostics) = diagnostics {
        diagnostics.http(crate::diagnostics::Stage::ChatHttp, status, response.headers());
    }
    if status != 200 {
        let bytes = bounded(response, 64 * 1024, cancel).await?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if let Some(diagnostics) = diagnostics {
            diagnostics.error_code(&value);
        }
        return Err(Error::Refused(
            super::refusal(status, &value).with_retry_after(retry_after),
        ));
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
        return Err(protocol(StreamCause::ContentType));
    }
    let mut source = response.bytes_stream();
    let mut decoder = Framer::default();
    let mut redactor = KeyRedactor::new(key);
    let mut seen_fields = BTreeSet::new();
    let mut tool_slots = BTreeMap::new();
    let mut names = BTreeMap::<usize, crate::redaction::Stream>::new();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut finish = None;
    let mut received = 0usize;
    loop {
        let next = tokio::select! { biased; _=cancel.cancelled()=>return Err(Error::Cancelled), next=source.next()=>next, };
        let Some(bytes) = next else {
            return Err(protocol(StreamCause::Eof));
        };
        let bytes = bytes.map_err(|_| transport())?;
        received = received
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_BODY)
            .ok_or(Error::TooLarge)?;
        for byte in bytes {
            let Some(Frame::Data(data)) = decoder.push(byte) else {
                continue;
            };
            if data == b"[DONE]" {
                let finish_reason = finish.ok_or_else(|| terminal(TerminalCheck::DoneWithoutFinish))?;
                if terminal_usage.is_none() {
                    return Err(terminal(TerminalCheck::DoneWithoutUsage));
                }
                let calls: Vec<Value> = tool_slots.into_values().collect();
                return Ok(Completed {
                    assistant: json!({"role":"assistant","content":text,"reasoning_content":reasoning,"tool_calls":calls}),
                    native_blocks: vec![],
                    usage: None,
                    finish_reason,
                });
            }
            // Final usage must be the last semantic frame before [DONE]. A
            // conflicting/later frame invalidates its authority for settlement.
            if terminal_usage.take().is_some() {
                return Err(terminal(TerminalCheck::FrameAfterUsage));
            }
            let mut chunk: Value =
                serde_json::from_slice(&data).map_err(|_| protocol(StreamCause::FrameShape))?;
            drop(data);
            if !chunk.is_object() || chunk.get("error").is_some() {
                if let Some(diagnostics) = diagnostics {
                    diagnostics.error_code(&chunk);
                }
                return Err(protocol(StreamCause::FrameShape));
            }
            let choices = chunk["choices"]
                .as_array()
                .ok_or_else(|| protocol(StreamCause::FrameShape))?;
            if choices.len() > 1 || choices.first().is_some_and(|choice| choice["index"] != 0) {
                return Err(protocol(StreamCause::FrameShape));
            }
            if choices.is_empty() && chunk.get("usage").is_none_or(Value::is_null) {
                return Err(protocol(StreamCause::FrameShape));
            }
            if finish.is_some() && !choices.is_empty() {
                return Err(terminal(TerminalCheck::ChoicesAfterFinish));
            }
            let ending = chunk["choices"][0]["finish_reason"]
                .as_str()
                .map(|s| match s {
                    "stop" => Ok("stop"),
                    "tool_calls" => Ok("tool_calls"),
                    "length" => Ok("length"),
                    "content_filter" => Ok("content_filter"),
                    _ => Err(protocol(StreamCause::FrameShape)),
                })
                .transpose()?;
            let mut argument_parts = Vec::new();
            if !choices.is_empty() {
                if !chunk["choices"][0]["delta"].is_object() {
                    return Err(protocol(StreamCause::FrameShape));
                }
                for (name, field) in [
                    ("content", Field::Content),
                    ("reasoning_content", Field::Thinking),
                ] {
                    if let Some(value) = chunk["choices"][0]["delta"].get(name).filter(|v| !v.is_null()) {
                        let value = value.as_str().ok_or_else(|| protocol(StreamCause::FrameShape))?;
                        let safe = redactor.feed(field, value);
                        seen_fields.insert(field);
                        chunk["choices"][0]["delta"][name] = safe.into();
                    }
                }
                if let Some(calls) = chunk["choices"][0]["delta"]
                    .get_mut("tool_calls")
                    .filter(|calls| !calls.is_null())
                {
                    for call in calls
                        .as_array_mut()
                        .ok_or_else(|| protocol(StreamCause::ToolShape))?
                    {
                        let index = call["index"]
                            .as_u64()
                            .and_then(|i| usize::try_from(i).ok())
                            .filter(|i| *i < 1024)
                            .ok_or_else(|| protocol(StreamCause::ToolShape))?;
                        if let Some(name) = call["function"].get("name").filter(|v| !v.is_null()) {
                            let bytes = names
                                .entry(index)
                                .or_insert_with(|| crate::redaction::Stream::new(key))
                                .feed(
                                    name.as_str()
                                        .ok_or_else(|| protocol(StreamCause::ToolShape))?
                                        .as_bytes(),
                                );
                            call["function"]["name"] = String::from_utf8(bytes)
                                .map_err(|_| protocol(StreamCause::ToolShape))?
                                .into();
                        }
                        if let Some(args) = call["function"]
                            .as_object_mut()
                            .and_then(|function| function.remove("arguments"))
                        {
                            match args {
                                Value::String(args) => argument_parts.push((index, args)),
                                Value::Null => {}
                                _ => return Err(protocol(StreamCause::ToolArguments)),
                            }
                        }
                    }
                }
                chunk["choices"][0]["finish_reason"] = Value::Null;
            }
            chunk["model"] = alias.into();
            crate::redaction::value(&mut chunk, key);
            collect(&chunk, &mut text, &mut reasoning, &mut tool_slots)
                .map_err(|_| protocol(StreamCause::ToolShape))?;
            // Argument strings contain another JSON layer. Retain fragments
            // privately until the complete decoded value can be checked.
            for (index, args) in argument_parts {
                let call = tool_slots
                    .get_mut(&index)
                    .ok_or_else(|| protocol(StreamCause::ToolArguments))?;
                let Value::String(buffer) = &mut call["function"]["arguments"] else {
                    return Err(protocol(StreamCause::ToolArguments));
                };
                buffer.push_str(&args);
            }
            if let Some(value) = chunk.get("usage").filter(|v| !v.is_null()) {
                if let Some(diagnostics) = diagnostics {
                    diagnostics
                        .billing_source(super::PlanName::from_value(&value["opencode"]["billing_source"]));
                }
                // Accept a separate final usage frame or usage on a validated
                // finish frame, never an interim record during generation.
                if finish.is_none() && ending.is_none() {
                    return Err(terminal(TerminalCheck::UsageBeforeFinish));
                }
                *terminal_usage =
                    Some(Usage::parse(value).map_err(|_| terminal(TerminalCheck::UsageRecordInvalid))?);
            }
            if cancel.is_cancelled() || on(Event::Chunk(chunk)) == Control::Stop {
                return Err(Error::Cancelled);
            }
            if let Some(reason) = ending {
                for field in &seen_fields {
                    let tail = redactor.finish(*field);
                    if tail.is_empty() {
                        continue;
                    }
                    let delta = match *field {
                        Field::Content => json!({"content":tail}),
                        Field::Thinking => json!({"reasoning_content":tail}),
                        Field::ToolArguments(index) => {
                            json!({"tool_calls":[{"index":index,"function":{"arguments":tail}}]})
                        }
                    };
                    let chunk =
                        json!({"model":alias,"choices":[{"index":0,"delta":delta,"finish_reason":null}]});
                    collect(&chunk, &mut text, &mut reasoning, &mut tool_slots)
                        .map_err(|_| protocol(StreamCause::ToolShape))?;
                    if cancel.is_cancelled() || on(Event::Chunk(chunk)) == Control::Stop {
                        return Err(Error::Cancelled);
                    }
                }
                for (index, state) in &mut names {
                    let tail =
                        String::from_utf8(state.finish()).map_err(|_| protocol(StreamCause::ToolShape))?;
                    if tail.is_empty() {
                        continue;
                    }
                    let chunk = json!({"model":alias,"choices":[{"index":0,"delta":{"tool_calls":[{"index":index,"function":{"name":tail}}]},"finish_reason":null}]});
                    collect(&chunk, &mut text, &mut reasoning, &mut tool_slots)
                        .map_err(|_| protocol(StreamCause::ToolShape))?;
                    if cancel.is_cancelled() || on(Event::Chunk(chunk)) == Control::Stop {
                        return Err(Error::Cancelled);
                    }
                }
                // A length stop cannot authorize a tool execution. Discard
                // private argument fragments instead of parsing/publishing an
                // incomplete JSON value; still deliver the terminal transition
                // and consume final usage for authoritative settlement.
                if reason == "length" {
                    tool_slots.clear();
                }
                let mut arguments = Vec::new();
                for (index, call) in &tool_slots {
                    let raw = call["function"]["arguments"]
                        .as_str()
                        .ok_or_else(|| protocol(StreamCause::ToolArguments))?;
                    let decoded: Value =
                        serde_json::from_str(raw).map_err(|_| protocol(StreamCause::ToolArguments))?;
                    if echoes_credential(&decoded, key) {
                        return Err(protocol(StreamCause::ToolArguments));
                    }
                    arguments.push(json!({"index":index,"function":{"arguments":raw}}));
                }
                if !arguments.is_empty() {
                    let chunk = json!({"model":alias,"choices":[{"index":0,"delta":{"tool_calls":arguments},"finish_reason":null}]});
                    if cancel.is_cancelled() || on(Event::Chunk(chunk)) == Control::Stop {
                        return Err(Error::Cancelled);
                    }
                }
                let chunk = json!({"model":alias,"choices":[{"index":0,"delta":{},"finish_reason":reason}]});
                if cancel.is_cancelled() || on(Event::Chunk(chunk)) == Control::Stop {
                    return Err(Error::Cancelled);
                }
                finish = Some(reason);
            }
        }
    }
}
fn collect(
    chunk: &Value,
    text: &mut String,
    reasoning: &mut String,
    calls: &mut BTreeMap<usize, Value>,
) -> Result<(), Error> {
    let delta = &chunk["choices"][0]["delta"];
    if let Some(s) = delta["content"].as_str() {
        text.push_str(s);
    }
    if let Some(s) = delta["reasoning_content"].as_str() {
        reasoning.push_str(s);
    }
    if let Some(slots) = delta["tool_calls"].as_array() {
        for slot in slots {
            let index = slot["index"].as_u64().ok_or(Error::Protocol)? as usize;
            let call = calls
                .entry(index)
                .or_insert_with(|| json!({"type":"function","function":{"name":"","arguments":""}}));
            if let Some(id) = slot["id"].as_str() {
                call["id"] = id.into();
            }
            for field in ["name", "arguments"] {
                if let Some(value) = slot["function"][field].as_str() {
                    let old = call["function"][field].as_str().unwrap_or("");
                    call["function"][field] = format!("{old}{value}").into();
                }
            }
        }
    }
    Ok(())
}
