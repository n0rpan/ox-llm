//! A2: request body, reply assembly and the HTTP `Engine`. Owner: task A2. Signatures below are frozen; bodies are yours.
use crate::hints::{build_headers, new_idem_key};
use crate::info::{clear_hint, count_tokens, fetch_info_full, http_error_with, send_cancel, stream_error};
use crate::{post_sse, Flow, SseEvent, StreamOpts, Url};
use oxa_proto::{
    codes, write_json_str, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, Finish, Hints,
    OxaError, StreamEvent, ThinkingLevel, Timings, ToolCall, Usage, Value, WireMsg,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// The chat path on the engine (below the base path of the URL).
const CHAT_PATH: &str = "/v1/chat/completions";
/// Pause before retry `n` is `n * RETRY_STEP_MS`.
const RETRY_STEP_MS: u64 = 100;

/// Append the request body for `r` to `out`. Pure; the only body builder (W1-W3, W6).
///
/// Exact layout, no whitespace, this key order:
/// `{"model":M,"messages":[{"role":"system","content":<prefix.system_text()>},<each message's
/// WireMsg::write_json>..],"tools":<prefix.tools_json verbatim>,"stream":true,
/// "stream_options":{"include_usage":true},"temperature":0,"cache_prompt":true,
/// "max_tokens":N,"chat_template_kwargs":{"preserve_thinking":true,"enable_thinking":B}}`.
/// `"tools"` is omitted when `tools_json` is `[]`. Nothing else is ever added: no date, no
/// user field, no seed.
pub fn build_body(r: &ChatRequest, out: &mut Vec<u8>) {
    out.extend_from_slice(b"{\"model\":");
    write_json_str(r.model, out);
    out.extend_from_slice(b",\"messages\":[{\"role\":\"system\",\"content\":");
    write_json_str(&r.prefix.system_text(), out);
    out.push(b'}');
    for m in r.messages {
        out.push(b',');
        m.write_json(out);
    }
    out.push(b']');
    let tools = r.prefix.tools_json.trim();
    if !tools.is_empty() && tools != "[]" {
        out.extend_from_slice(b",\"tools\":");
        out.extend_from_slice(r.prefix.tools_json.as_bytes());
    }
    out.extend_from_slice(
        b",\"stream\":true,\"stream_options\":{\"include_usage\":true},\"temperature\":0,\"cache_prompt\":true,\"max_tokens\":",
    );
    out.extend_from_slice(r.max_tokens.to_string().as_bytes());
    out.extend_from_slice(b",\"chat_template_kwargs\":{");
    if r.preserve_thinking {
        out.extend_from_slice(b"\"preserve_thinking\":true,");
    }
    out.extend_from_slice(b"\"enable_thinking\":");
    out.extend_from_slice(if r.thinking { b"true" } else { b"false" });
    out.extend_from_slice(b"}}");
}

/// Builds an [`AssistantTurn`] from the `data:` chunks of one stream.
///
/// W2: `content` and `reasoning` are the concatenation of the deltas, untrimmed (`None` when
/// no delta of that kind arrived); each tool call's `args_raw` is the concatenation of its
/// argument deltas; ids and names are the server's.
#[derive(Debug, Default)]
pub struct ReplyAssembler {
    content: Option<String>,
    reasoning: Option<String>,
    calls: Vec<oxa_proto::ToolCall>,
    finish: Option<oxa_proto::Finish>,
    usage: Option<oxa_proto::Usage>,
    timings: Option<oxa_proto::Timings>,
    model: String,
    /// Wire `index` of each entry of `calls`, in the same order.
    indices: Vec<usize>,
}

impl ReplyAssembler {
    pub fn new() -> ReplyAssembler {
        ReplyAssembler::default()
    }
    /// Feed one `data:` JSON chunk; returns the stream events it contains, in order.
    /// Malformed JSON = `stream_broken`. An error object (`{"error":{..}}`, the controller's
    /// `event: error` after early headers or the engine's own in-stream error) is classified
    /// like a status reply (F3): `exceed_context_size_error` -> `context_overflow`, a status in
    /// `code` -> `engine_http` / `engine_busy` naming `HTTP <status>`, none -> `stream_broken`.
    pub fn chunk(&mut self, data: &str) -> Result<Vec<StreamEvent>, OxaError> {
        let v: Value =
            serde_json::from_str(data).map_err(|e| broken(format!("malformed stream chunk: {e}")))?;
        let obj = v
            .as_object()
            .ok_or_else(|| broken("a stream chunk is not a JSON object".to_string()))?;
        if let Some(err) = obj.get("error") {
            // F3: classified like a status reply (overflow, busy, 4xx final, 5xx retryable).
            return Err(stream_error(err));
        }
        if let Some(m) = obj.get("model").and_then(Value::as_str) {
            self.model = m.to_string();
        }
        if let Some(u) = obj.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(parse_usage(u)?);
        }
        if let Some(t) = obj.get("timings").filter(|t| !t.is_null()) {
            self.timings = Some(parse_timings(t));
        }

        let mut events = Vec::new();
        let choices = match obj.get("choices").and_then(Value::as_array) {
            Some(c) => c,
            None => return Ok(events),
        };
        for choice in choices {
            if let Some(reason) = choice.get("finish_reason").filter(|f| !f.is_null()) {
                self.finish = Some(parse_finish(reason)?);
            }
            let delta = match choice.get("delta") {
                Some(d) => d,
                None => continue,
            };
            if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str) {
                push_text(&mut self.reasoning, r);
                if !r.is_empty() {
                    events.push(StreamEvent::Reasoning(r.to_string()));
                }
            }
            if let Some(c) = delta.get("content").and_then(Value::as_str) {
                push_text(&mut self.content, c);
                if !c.is_empty() {
                    events.push(StreamEvent::Text(c.to_string()));
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for (pos, tc) in calls.iter().enumerate() {
                    let index = tc
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|i| usize::try_from(i).ok())
                        .unwrap_or(pos);
                    let slot = self.slot(index);
                    let id = tc.get("id").and_then(Value::as_str).filter(|s| !s.is_empty());
                    let name = tc
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty());
                    let args = tc
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if let Some(id) = id {
                        self.calls[slot].id = id.to_string();
                    }
                    if let Some(name) = name {
                        self.calls[slot].name = name.to_string();
                    }
                    self.calls[slot].args_raw.push_str(args);
                    if id.is_some() || name.is_some() || !args.is_empty() {
                        events.push(StreamEvent::ToolArgs {
                            index,
                            id: id.map(str::to_string),
                            name: name.map(str::to_string),
                            delta: args.to_string(),
                        });
                    }
                }
            }
        }
        Ok(events)
    }

    /// Parse a comment line (M2, S2 / F10); other comments -> `None`.
    ///
    /// * `prefill <done>/<total> [<s>s ..]` (the engine: `: prefill 12288/28500 361s`) ->
    ///   `Prefill`. The first whitespace token after the slash is the total; a `<n>s` field is
    ///   the engine's own seconds, else `secs` (given by the caller) is used; later fields are
    ///   ignored.
    /// * `queued [pos=<n>] wait_s=<s>` (one format for the engine and the controller; key=value
    ///   fields, unknown keys ignored) -> `Queued`. Legacy forms: `queued <n>` (the controller:
    ///   a bare integer is the position) and `queued <n>s` (the engine: seconds waited). A
    ///   missing `wait_s` is the caller's `secs`.
    pub fn comment(&self, comment: &str, secs: u32) -> Option<StreamEvent> {
        let text = comment.trim();
        if let Some(rest) = text.strip_prefix("prefill ") {
            let (done, tail) = rest.split_once('/')?;
            let mut fields = tail.split_whitespace();
            let total = fields.next()?.parse().ok()?;
            let own = fields.find_map(|f| f.strip_suffix('s').and_then(|n| n.parse::<u32>().ok()));
            return Some(StreamEvent::Prefill {
                done: done.trim().parse().ok()?,
                total,
                secs: own.unwrap_or(secs),
            });
        }
        let rest = match text.strip_prefix("queued") {
            Some(r) if r.is_empty() || r.starts_with(char::is_whitespace) => r,
            _ => return None,
        };
        let mut pos = None;
        let mut wait_s = None;
        for field in rest.split_whitespace() {
            match field.split_once('=') {
                Some(("pos", v)) => pos = v.parse().ok(),
                Some(("wait_s", v)) => wait_s = v.parse().ok(),
                Some(_) => {}
                None => {
                    if let Some(n) = field.strip_suffix('s').and_then(|n| n.parse().ok()) {
                        wait_s = Some(n);
                    } else if let Ok(n) = field.parse() {
                        pos = Some(n);
                    }
                }
            }
        }
        Some(StreamEvent::Queued {
            pos,
            wait_s: wait_s.unwrap_or(secs),
        })
    }

    /// Finish after `[DONE]`. No `finish_reason` or no usage = `stream_broken`.
    pub fn finish(self, level: ThinkingLevel) -> Result<AssistantTurn, OxaError> {
        let finish = self
            .finish
            .ok_or_else(|| broken("the stream ended without a finish_reason".to_string()))?;
        let usage = self
            .usage
            .ok_or_else(|| broken("the stream ended without usage".to_string()))?;
        if let Some(call) = self.calls.iter().find(|c| c.id.is_empty() || c.name.is_empty()) {
            return Err(broken(format!(
                "a tool call arrived without an id or a name (id {:?}, name {:?})",
                call.id, call.name
            )));
        }
        Ok(AssistantTurn {
            msg: WireMsg {
                role: oxa_proto::Role::Assistant,
                content: self.content,
                reasoning: self.reasoning,
                tool_calls: self.calls,
                tool_call_id: None,
            },
            finish,
            usage,
            timings: self.timings,
            model: self.model,
            level,
        })
    }

    /// Position in `calls` of the tool call with wire `index`, created on first sight.
    fn slot(&mut self, index: usize) -> usize {
        if let Some(p) = self.indices.iter().position(|&i| i == index) {
            return p;
        }
        self.indices.push(index);
        self.calls.push(ToolCall {
            id: String::new(),
            name: String::new(),
            args_raw: String::new(),
        });
        self.calls.len() - 1
    }
}

/// Append a delta to an optional text. A delta of the kind arrived, so the field becomes `Some`
/// even when the delta is empty.
fn push_text(field: &mut Option<String>, delta: &str) {
    field.get_or_insert_with(String::new).push_str(delta);
}

fn broken(msg: String) -> OxaError {
    OxaError::new(codes::STREAM_BROKEN, msg)
}

fn parse_usage(u: &Value) -> Result<Usage, OxaError> {
    let count = |v: Option<&Value>| v.and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
    let prompt =
        count(u.get("prompt_tokens")).ok_or_else(|| broken("usage has no prompt_tokens".to_string()))?;
    let completion = count(u.get("completion_tokens"))
        .ok_or_else(|| broken("usage has no completion_tokens".to_string()))?;
    // B6: a server that does not report `cached_tokens` is not a server with nothing cached:
    // the reply carries `cached_missing` and no cache miss is derived from it.
    let reported = count(u.pointer("/prompt_tokens_details/cached_tokens"));
    Ok(Usage {
        prompt,
        completion,
        cached: reported.unwrap_or(0),
        cached_missing: reported.is_none(),
    })
}

fn parse_timings(t: &Value) -> Timings {
    let count = |key: &str| {
        t.get(key)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0)
    };
    let ms = |key: &str| t.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    Timings {
        cache_n: count("cache_n"),
        prompt_n: count("prompt_n"),
        prompt_ms: ms("prompt_ms"),
        predicted_n: count("predicted_n"),
        predicted_ms: ms("predicted_ms"),
        snap_pos: t
            .get("snap_pos")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        snap_asked: t.get("snap_pos").is_some(),
    }
}

fn parse_finish(v: &Value) -> Result<Finish, OxaError> {
    match v.as_str() {
        Some("stop") => Ok(Finish::Stop),
        Some("tool_calls") => Ok(Finish::ToolCalls),
        Some("length") => Ok(Finish::Length),
        _ => Err(broken(format!("unknown finish_reason {v}"))),
    }
}

/// Sleep `ms`, polling `cancel` so a cancel never waits out a backoff.
fn pause(cancel: &CancelToken, ms: u64) -> Result<(), OxaError> {
    let end = Instant::now() + Duration::from_millis(ms);
    loop {
        if cancel.is_cancelled() {
            return Err(OxaError::new(codes::ABORTED, "request cancelled"));
        }
        let now = Instant::now();
        if now >= end {
            return Ok(());
        }
        thread::sleep((end - now).min(Duration::from_millis(20)));
    }
}

/// Errors that a new attempt may cure: the engine was not reachable, or the connection ended or
/// stalled. Only used before the first data byte. `engine_busy` is not retried here: the wait
/// (`Retry-After`) is the core's retry.
fn retryable(e: &OxaError) -> bool {
    matches!(e.code, codes::ENGINE_DOWN | codes::STREAM_BROKEN | codes::TIMEOUT)
}

/// The real engine client.
#[derive(Debug, Clone)]
pub struct HttpEngine {
    url: Url,
    api_key: Option<String>,
    opts: StreamOpts,
    /// Retries before the first data byte (default 2).
    connect_retries: u32,
    /// Capability-gated proposal (cancel-contract.md section 7): the engine advertised the cancel notice (`CANCEL_HINT`)
    /// in the last [`Engine::info`] of this endpoint. Shared by the clones of this engine.
    cancel_ok: Arc<AtomicBool>,
}

impl HttpEngine {
    pub fn new(base_url: &str, api_key: Option<String>) -> Result<HttpEngine, OxaError> {
        let url = Url::parse(base_url)?;
        // P3: an engine that is not on loopback only with `oxa.engine.allowRemote`.
        oxa_proto::check_engine_host(&url.host)?;
        Ok(HttpEngine {
            url,
            api_key: api_key.filter(|k| !k.is_empty()),
            opts: StreamOpts::default(),
            connect_retries: 2,
            cancel_ok: Arc::new(AtomicBool::new(false)),
        })
    }
    pub fn with_opts(mut self, opts: StreamOpts, connect_retries: u32) -> HttpEngine {
        self.opts = opts;
        self.connect_retries = connect_retries;
        self
    }
}

impl HttpEngine {
    /// Capability-gated proposal (cancel-contract.md section 7): a request ended by a deliberate cancel (`cancel` fired,
    /// the request was not finished) tells the engine so, once, when the engine advertised it.
    /// Connection failures and server-side closes are not cancels and send nothing: the
    /// engine's replay grace is what a retry with the same key needs there. Returns `e`.
    fn abandoned(&self, e: OxaError, cancel: &CancelToken, key: &str, session: &str) -> OxaError {
        if e.code == codes::ABORTED && cancel.is_cancelled() && self.cancel_ok.load(Ordering::SeqCst) {
            send_cancel(&self.url, self.api_key.as_deref(), key, session);
        }
        e
    }
}

impl Engine for HttpEngine {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        let (info, cancel) = fetch_info_full(&self.url, self.api_key.as_deref(), target)?;
        self.cancel_ok.store(cancel, Ordering::SeqCst);
        Ok(info)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        count_tokens(&self.url, self.api_key.as_deref(), text)
    }
    fn clear_hint(&self, hint: &str) -> Result<(), OxaError> {
        clear_hint(&self.url, self.api_key.as_deref(), hint)
    }
    fn count_prompt(&self, r: &ChatRequest) -> Result<Option<u64>, OxaError> {
        let mut body = Vec::new();
        build_body(r, &mut body);
        crate::info::count_prompt(&self.url, self.api_key.as_deref(), &body)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        let mut body = Vec::new();
        build_body(r, &mut body);

        // One idempotency key for the whole logical request: every attempt sends it (H2).
        let key = if r.hints.idem_key.is_empty() {
            new_idem_key()
        } else {
            r.hints.idem_key.to_string()
        };
        let hints = Hints {
            idem_key: &key,
            ..r.hints
        };
        let mut headers = build_headers(&hints);
        if let Some(k) = self.api_key.as_deref().filter(|k| !k.is_empty()) {
            headers.push(("Authorization", format!("Bearer {k}")));
        }
        let level = if r.thinking {
            ThinkingLevel::Medium
        } else {
            ThinkingLevel::Off
        };
        let started = Instant::now();
        let mut attempt = 0u32;
        // Cancelled before anything was sent: nothing reached the engine, nothing to undo.
        if cancel.is_cancelled() {
            return Err(OxaError::new(codes::ABORTED, "request cancelled"));
        }

        loop {
            let mut asm = ReplyAssembler::new();
            asm.model = r.model.to_string();
            // Set once any `data:` or `[DONE]` line arrived: a retry is never allowed after it.
            let mut began = false;
            // Set once a stream event reached `on`: an error after it is a cut reply (F3).
            let mut emitted = false;
            let mut failure: Option<OxaError> = None;
            let res = post_sse(
                &self.url,
                CHAT_PATH,
                &headers,
                &body,
                &self.opts,
                cancel,
                &mut |ev| {
                    match ev {
                        SseEvent::Data(data) => {
                            began = true;
                            match asm.chunk(&data) {
                                Ok(events) => {
                                    for e in events {
                                        emitted = true;
                                        on(e);
                                    }
                                }
                                Err(e) => {
                                    // An error after reply data is a cut reply, never replayed
                                    // (the core retries `stream_broken` only before data).
                                    failure = Some(if emitted && e.code != codes::STREAM_BROKEN {
                                        broken(format!("the reply was cut: {}", e.message))
                                    } else {
                                        e
                                    });
                                    return Flow::Stop;
                                }
                            }
                        }
                        SseEvent::Comment(c) => {
                            let secs = u32::try_from(started.elapsed().as_secs()).unwrap_or(u32::MAX);
                            if let Some(p) = asm.comment(&c, secs) {
                                on(p);
                            }
                        }
                        SseEvent::Done => began = true,
                    }
                    Flow::Continue
                },
            );

            let outcome = match (failure, res) {
                (Some(e), _) => Err(e),
                (None, Err(e)) => Err(e),
                (None, Ok(end)) => {
                    if !(200..300).contains(&end.status) {
                        let text = end.error_body.unwrap_or_default();
                        Err(http_error_with(end.status, text.as_bytes(), end.retry_after))
                    } else if !end.done {
                        Err(broken("the engine closed the stream before [DONE]".to_string()))
                    } else {
                        asm.finish(level)
                    }
                }
            };

            match outcome {
                Ok(turn) => return Ok(turn),
                Err(e) => {
                    if !began && retryable(&e) && attempt < self.connect_retries {
                        attempt += 1;
                        if let Err(p) = pause(cancel, RETRY_STEP_MS * u64::from(attempt)) {
                            return Err(self.abandoned(p, cancel, &key, r.hints.session));
                        }
                        continue;
                    }
                    return Err(self.abandoned(e, cancel, &key, r.hints.session));
                }
            }
        }
    }
}

#[cfg(test)]
mod usage_tests {
    use super::parse_usage;
    use serde_json::json;

    /// B6: the oxe engine reports `cached_tokens` always (0 included); a server that leaves it
    /// out is "not reported", never 0 cached. `completion_tokens` (reasoning included, as the
    /// engine counts it) is taken as it is; `completion_tokens_details` is not needed.
    #[test]
    fn missing_cached_tokens_is_not_zero_cached() {
        let full = json!({"prompt_tokens": 10, "completion_tokens": 5,
            "prompt_tokens_details": {"cached_tokens": 0},
            "completion_tokens_details": {"reasoning_tokens": 3}});
        let u = parse_usage(&full).unwrap();
        assert_eq!(
            (u.prompt, u.completion, u.cached, u.cached_missing),
            (10, 5, 0, false)
        );
        let bare = json!({"prompt_tokens": 10, "completion_tokens": 5});
        let u = parse_usage(&bare).unwrap();
        assert_eq!((u.cached, u.cached_missing), (0, true));
        let null = json!({"prompt_tokens": 10, "completion_tokens": 5, "prompt_tokens_details": null});
        assert!(parse_usage(&null).unwrap().cached_missing);
    }
}
