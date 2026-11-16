//! Chat-completions transport over the gateway's existing hardened HTTP pool,
//! SSE decoder and decoded-field redactor. No admission, credentials or queue.
use super::{Model, Refusal, SessionHint, Usage};
use crate::{
    anthropic_client::{Control, KeyRedactor},
    anthropic_stream::{Completed, Field, Redactor},
    secrets::Key,
    sse::Framer,
};
use futures_util::StreamExt;
use oxa_proto::CancelToken;
use reqwest::{
    header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT},
    Client, Url,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

const MAX_BODY: usize = 64 * 1024 * 1024;
const MAX_DISCOVERY: usize = 4 * 1024 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<R = Refusal> {
    Endpoint,
    Request,
    Transport,
    Protocol,
    TooLarge,
    Cancelled,
    Refused(R),
}
pub enum Event {
    Sending,
    Chunk(Value),
}

fn bearer<R>(key: &Key) -> Result<HeaderValue, Error<R>> {
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
async fn bounded<R>(
    mut response: reqwest::Response,
    max: usize,
    cancel: &CancelToken,
) -> Result<Vec<u8>, Error<R>> {
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
pub async fn models(
    client: &Client,
    base: &str,
    key: &Key,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<Vec<Model>, Error> {
    let mut url = super::endpoint(base).map_err(|_| Error::Endpoint)?;
    url.path_segments_mut()
        .map_err(|_| Error::Endpoint)?
        .pop_if_empty()
        .push("v1")
        .push("models");
    let operation = async {
        let request = client
            .get(url)
            .header(USER_AGENT, concat!("oxa/", env!("CARGO_PKG_VERSION")))
            .header(AUTHORIZATION, bearer(key)?);
        let response = tokio::select! { biased; _=cancel.cancelled()=>return Err(Error::Cancelled), response=request.send()=>response.map_err(|_|Error::Transport)?, };
        let status = response.status().as_u16();
        let bytes = bounded(response, MAX_DISCOVERY, cancel).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
        if status != 200 {
            return Err(Error::Refused(super::refusal(status, &value)));
        }
        // Discovery becomes public catalog metadata. Reject an echoed secret
        // anywhere in decoded fields, including names and nested hints; never
        // redact/rewrite an upstream routing identifier into another model.
        if echoes_credential(&value, key) {
            return Err(Error::Protocol);
        }
        super::models(&value, base).map_err(|_| Error::Protocol)
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

/// Provider-owned preparation for the one shared Chat Completions transport.
/// The caller retains routing/privacy policy, headers and typed usage/refusals;
/// this loop retains endpoint hygiene, framing, redaction and terminal ordering.
pub(crate) struct PreparedChat<'a, U, R> {
    pub endpoint: Url,
    pub alias: &'a str,
    pub payload: &'a Value,
    pub key: &'a Key,
    pub headers: HeaderMap,
    pub parse_usage: fn(&Value) -> Option<U>,
    pub refusal: fn(u16, &Value) -> R,
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
    *terminal_usage = None;
    let Chat {
        model,
        alias,
        payload,
        key,
        hint,
    } = chat;
    let endpoint = model.chat_endpoint().map_err(|_| Error::Endpoint)?;
    let payload = model.request(payload).map_err(|_| Error::Request)?;
    stream_prepared(
        client,
        PreparedChat {
            endpoint,
            alias,
            payload: &payload,
            key,
            headers: hint.headers(),
            parse_usage: |value| Usage::parse(value).ok(),
            refusal: super::refusal,
        },
        cancel,
        on,
        terminal_usage,
    )
    .await
}

pub(crate) async fn stream_prepared<U, R, F>(
    client: &Client,
    chat: PreparedChat<'_, U, R>,
    cancel: &CancelToken,
    on: &mut F,
    terminal_usage: &mut Option<U>,
) -> Result<Completed, Error<R>>
where
    F: FnMut(Event) -> Control + ?Sized,
{
    *terminal_usage = None;
    let PreparedChat {
        endpoint,
        alias,
        payload,
        key,
        headers,
        parse_usage,
        refusal,
    } = chat;
    let endpoint = super::endpoint(endpoint.as_str()).map_err(|_| Error::Endpoint)?;
    let body = serde_json::to_vec(payload).map_err(|_| Error::Request)?;
    if body.len() > MAX_BODY {
        return Err(Error::TooLarge);
    }
    if cancel.is_cancelled() || on(Event::Sending) == Control::Stop {
        return Err(Error::Cancelled);
    }
    let request = client
        .post(endpoint)
        .headers(headers)
        .header(AUTHORIZATION, bearer(key)?)
        .header(CONTENT_TYPE, "application/json")
        .header("accept", "text/event-stream")
        .body(body);
    let response = tokio::select! { biased; _=cancel.cancelled()=>return Err(Error::Cancelled), response=request.send()=>response.map_err(|_|Error::Transport)?, };
    let status = response.status().as_u16();
    if status != 200 {
        let bytes = bounded(response, 64 * 1024, cancel).await?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        return Err(Error::Refused(refusal(status, &value)));
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
            return Err(Error::Protocol);
        };
        let bytes = bytes.map_err(|_| Error::Transport)?;
        received = received
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_BODY)
            .ok_or(Error::TooLarge)?;
        for byte in bytes {
            let Some(data) = decoder.push(byte) else { continue };
            if data == b"[DONE]" {
                let finish_reason = finish.ok_or(Error::Protocol)?;
                if terminal_usage.is_none() {
                    return Err(Error::Protocol);
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
                return Err(Error::Protocol);
            }
            let mut chunk: Value = serde_json::from_slice(&data).map_err(|_| Error::Protocol)?;
            drop(data);
            if !chunk.is_object() || chunk.get("error").is_some() {
                return Err(Error::Protocol);
            }
            let choices = chunk["choices"].as_array().ok_or(Error::Protocol)?;
            if choices.len() > 1 || choices.first().is_some_and(|choice| choice["index"] != 0) {
                return Err(Error::Protocol);
            }
            if choices.is_empty() && chunk.get("usage").is_none_or(Value::is_null) {
                return Err(Error::Protocol);
            }
            if finish.is_some() && !choices.is_empty() {
                return Err(Error::Protocol);
            }
            let ending = chunk["choices"][0]["finish_reason"]
                .as_str()
                .map(|s| match s {
                    "stop" => Ok("stop"),
                    "tool_calls" => Ok("tool_calls"),
                    "length" => Ok("length"),
                    "content_filter" => Ok("content_filter"),
                    _ => Err(Error::Protocol),
                })
                .transpose()?;
            let mut argument_parts = Vec::new();
            if !choices.is_empty() {
                if !chunk["choices"][0]["delta"].is_object() {
                    return Err(Error::Protocol);
                }
                for (name, field) in [
                    ("content", Field::Content),
                    ("reasoning_content", Field::Thinking),
                ] {
                    if let Some(value) = chunk["choices"][0]["delta"].get(name).filter(|v| !v.is_null()) {
                        let value = value.as_str().ok_or(Error::Protocol)?;
                        let safe = redactor.feed(field, value);
                        seen_fields.insert(field);
                        chunk["choices"][0]["delta"][name] = safe.into();
                    }
                }
                if let Some(calls) = chunk["choices"][0]["delta"].get_mut("tool_calls") {
                    for call in calls.as_array_mut().ok_or(Error::Protocol)? {
                        let index = call["index"]
                            .as_u64()
                            .and_then(|i| usize::try_from(i).ok())
                            .filter(|i| *i < 1024)
                            .ok_or(Error::Protocol)?;
                        if let Some(name) = call["function"].get("name").filter(|v| !v.is_null()) {
                            let bytes = names
                                .entry(index)
                                .or_insert_with(|| crate::redaction::Stream::new(key))
                                .feed(name.as_str().ok_or(Error::Protocol)?.as_bytes());
                            call["function"]["name"] =
                                String::from_utf8(bytes).map_err(|_| Error::Protocol)?.into();
                        }
                        if let Some(args) = call["function"]
                            .as_object_mut()
                            .and_then(|function| function.remove("arguments"))
                        {
                            match args {
                                Value::String(args) => argument_parts.push((index, args)),
                                Value::Null => {}
                                _ => return Err(Error::Protocol),
                            }
                        }
                    }
                }
                chunk["choices"][0]["finish_reason"] = Value::Null;
            }
            chunk["model"] = alias.into();
            crate::redaction::value(&mut chunk, key);
            collect(&chunk, &mut text, &mut reasoning, &mut tool_slots)?;
            // Argument strings contain another JSON layer. Retain fragments
            // privately until the complete decoded value can be checked.
            for (index, args) in argument_parts {
                let call = tool_slots.get_mut(&index).ok_or(Error::Protocol)?;
                let Value::String(buffer) = &mut call["function"]["arguments"] else {
                    return Err(Error::Protocol);
                };
                buffer.push_str(&args);
            }
            if let Some(value) = chunk.get("usage").filter(|v| !v.is_null()) {
                // Accept a separate final usage frame or usage on a validated
                // finish frame, never an interim record during generation.
                if finish.is_none() && ending.is_none() {
                    return Err(Error::Protocol);
                }
                *terminal_usage = Some(parse_usage(value).ok_or(Error::Protocol)?);
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
                    collect(&chunk, &mut text, &mut reasoning, &mut tool_slots)?;
                    if cancel.is_cancelled() || on(Event::Chunk(chunk)) == Control::Stop {
                        return Err(Error::Cancelled);
                    }
                }
                for (index, state) in &mut names {
                    let tail = String::from_utf8(state.finish()).map_err(|_| Error::Protocol)?;
                    if tail.is_empty() {
                        continue;
                    }
                    let chunk = json!({"model":alias,"choices":[{"index":0,"delta":{"tool_calls":[{"index":index,"function":{"name":tail}}]},"finish_reason":null}]});
                    collect(&chunk, &mut text, &mut reasoning, &mut tool_slots)?;
                    if cancel.is_cancelled() || on(Event::Chunk(chunk)) == Control::Stop {
                        return Err(Error::Cancelled);
                    }
                }
                let mut arguments = Vec::new();
                for (index, call) in &tool_slots {
                    let raw = call["function"]["arguments"].as_str().ok_or(Error::Protocol)?;
                    let decoded: Value = serde_json::from_str(raw).map_err(|_| Error::Protocol)?;
                    if echoes_credential(&decoded, key) {
                        return Err(Error::Protocol);
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
fn collect<R>(
    chunk: &Value,
    text: &mut String,
    reasoning: &mut String,
    calls: &mut BTreeMap<usize, Value>,
) -> Result<(), Error<R>> {
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
