//! A2: request body, reply assembly and the HTTP `Engine`.
use crate::hints::{build_headers, new_idem_key};
use crate::info::{clear_hint, count_tokens, fetch_info_full, http_error_with, send_cancel, stream_error};
use crate::{post_sse, Flow, SseEvent, StreamOpts, Url};
use oxa_proto::{
    codes, write_json_str, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, Finish, Hints,
    OxaError, StreamEvent, ThinkingLevel, Timings, ToolCall, Usage, Value, WireMsg,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
/// `"tools"` is omitted when `tools_json` is `[]`. The builder adds no automatic date or
/// user field. With `r.sampling` set (opt-in, `oxa.sampling.mode = model`) the
/// `0` becomes `T,"top_p":P,"top_k":K,"min_p":M,"presence_penalty":X` (+ `,"seed":S` when
/// one is fixed), in place, before `"cache_prompt"`. A non-null `thinking_budget` is
/// added only for local thinking requests with positive output room.
pub fn build_body(r: &ChatRequest, out: &mut Vec<u8>) {
    build_body_for(r, false, out);
}

/// The `reasoning_effort` of a cloud request: the level itself, `xhigh` and `max` capped at the
/// highest effort the OpenAI-compatible wire has.
fn reasoning_effort(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "none",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "high",
    }
}

/// [`build_body`] for a local engine (`remote == false`) or a cloud model routed through the
/// controller (`remote == true`, Wave D D4). A remote body never carries local-only prefix,
/// cache or template fields; its thinking switch is the standard `reasoning_effort`.
pub fn build_body_for(r: &ChatRequest, remote: bool, out: &mut Vec<u8>) {
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
    out.extend_from_slice(b",\"stream\":true,\"stream_options\":{\"include_usage\":true},\"temperature\":");
    match &r.sampling {
        None => out.push(b'0'),
        Some(s) => {
            let mut keys = format!(
                "{},\"top_p\":{},\"top_k\":{},\"min_p\":{},\"presence_penalty\":{}",
                s.temperature, s.top_p, s.top_k, s.min_p, s.presence_penalty
            );
            if let Some(seed) = s.seed {
                keys.push_str(&format!(",\"seed\":{seed}"));
            }
            out.extend_from_slice(keys.as_bytes());
        }
    }
    if remote {
        if let Some(schema) = r.output_schema {
            out.extend_from_slice(b",\"response_format\":");
            let format = serde_json::json!({"type":"json_schema", "json_schema":{
                "name":"oxa_output", "schema":schema
            }});
            out.extend_from_slice(format.to_string().as_bytes());
        }
        out.extend_from_slice(b",\"max_tokens\":");
        out.extend_from_slice(r.max_tokens.to_string().as_bytes());
        out.extend_from_slice(b",\"reasoning_effort\":\"");
        out.extend_from_slice(reasoning_effort(r.effective_level()).as_bytes());
        out.push(b'"');
        out.push(b'}');
        return;
    }
    if r.thinking && r.max_tokens > 0 {
        if let Some(budget) = r.thinking_budget {
            out.extend_from_slice(b",\"thinking_budget\":");
            out.extend_from_slice(budget.to_string().as_bytes());
        }
    }
    out.extend_from_slice(b",\"cache_prompt\":true,\"max_tokens\":");
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
    /// Inside a `<think>` block that arrived as `content` text.
    in_think: bool,
    /// A think block has closed: the rest of the answer is literal text.
    think_done: bool,
    /// Held-back tail of `content` that may be the start of a think tag.
    held: String,
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
        self.chunk_value(&v)
    }
    /// Assemble already-decoded gateway chunks without encoding and reparsing them.
    pub fn chunk_value(&mut self, v: &Value) -> Result<Vec<StreamEvent>, OxaError> {
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
                self.content_delta(c, &mut events);
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
                        // Names arrive whole: a repeat is idempotent, only a real fragment appends.
                        let cur = &mut self.calls[slot].name;
                        if cur.is_empty() || cur == name {
                            *cur = name.to_string();
                        } else {
                            cur.push_str(name);
                        }
                    }
                    self.calls[slot].args_raw.push_str(args);
                    if id.is_some() || name.is_some() || !args.is_empty() {
                        events.push(StreamEvent::ToolArgs {
                            index,
                            id: id.map(str::to_string),
                            name: name.map(|_| self.calls[slot].name.clone()),
                            delta: args.to_string(),
                        });
                    }
                }
            }
        }
        Ok(events)
    }

    /// Routes a `content` delta: `<think>..</think>` at the start of the text is reasoning
    /// (an engine that does not split it into `reasoning_content`), the rest is answer text.
    fn content_delta(&mut self, c: &str, events: &mut Vec<StreamEvent>) {
        const OPEN: &str = "<think>";
        const CLOSE: &str = "</think>";
        let mut buf = std::mem::take(&mut self.held);
        buf.push_str(c);
        loop {
            if self.in_think {
                if let Some(i) = buf.find(CLOSE) {
                    self.emit_reasoning(&buf[..i], events);
                    buf = buf[i + CLOSE.len()..].to_string();
                    self.in_think = false;
                    self.think_done = true;
                    continue;
                }
                let keep = tag_prefix_len(&buf, CLOSE);
                let cut = buf.len() - keep;
                self.emit_reasoning(&buf[..cut], events);
                self.held = buf[cut..].to_string();
                return;
            }
            let at_start = !self.think_done && self.content.as_deref().is_none_or(|t| t.trim().is_empty());
            if at_start {
                let lead = buf.len() - buf.trim_start().len();
                let rest = &buf[lead..];
                if let Some(after) = rest.strip_prefix(OPEN) {
                    buf = after.to_string();
                    self.in_think = true;
                    continue;
                }
                if rest.is_empty() || OPEN.starts_with(rest) {
                    self.held = buf;
                    return;
                }
            }
            self.emit_text(&buf, events);
            return;
        }
    }

    fn emit_reasoning(&mut self, r: &str, events: &mut Vec<StreamEvent>) {
        if !r.is_empty() {
            push_text(&mut self.reasoning, r);
            events.push(StreamEvent::Reasoning(r.to_string()));
        }
    }

    fn emit_text(&mut self, t: &str, events: &mut Vec<StreamEvent>) {
        if !t.is_empty() {
            push_text(&mut self.content, t);
            events.push(StreamEvent::Text(t.to_string()));
        }
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

    /// Delivers the held tail (a literal `<` or `<thi`, or a partial `</think>` at a length
    /// stop) once the stream has ended. The events must reach the stream callback; the caller
    /// calls this before [`Self::finish`], which then has nothing left to route.
    pub fn flush(&mut self) -> Vec<StreamEvent> {
        let held = std::mem::take(&mut self.held);
        let mut events = Vec::new();
        if self.in_think {
            self.emit_reasoning(&held, &mut events);
        } else {
            self.emit_text(&held, &mut events);
        }
        events
    }

    /// Finish after `[DONE]`. No `finish_reason` or no usage = `stream_broken`.
    pub fn finish(self, level: ThinkingLevel) -> Result<AssistantTurn, OxaError> {
        self.finish_inner(level, false)
    }

    /// As `finish`, but a reply with a valid finish and no usage completes with unknown usage
    /// through the embedded gateway ledger as `usage_missing`. HTTP still requires usage.
    pub fn finish_unreported_usage_ok(self, level: ThinkingLevel) -> Result<AssistantTurn, OxaError> {
        self.finish_inner(level, true)
    }

    fn finish_inner(mut self, level: ThinkingLevel, usage_optional: bool) -> Result<AssistantTurn, OxaError> {
        // Any held tail not yet flushed still joins the assembled reply; its events are dropped
        // here because the stream loop delivers them through `flush` first.
        let _ = self.flush();
        let finish = self
            .finish
            .ok_or_else(|| broken("the stream ended without a finish_reason".to_string()))?;
        let usage = match self.usage {
            Some(usage) => usage,
            None if usage_optional => Usage::unreported(),
            None => return Err(broken("the stream ended without usage".to_string())),
        };
        if let Some(call) = self.calls.iter().find(|c| c.id.is_empty() || c.name.is_empty()) {
            return Err(broken(format!(
                "a tool call arrived without an id or a name (id {:?}, name {:?})",
                call.id, call.name
            )));
        }
        Ok(AssistantTurn {
            msg: WireMsg {
                images: Vec::new(),
                role: oxa_proto::Role::Assistant,
                content: self.content,
                reasoning: self.reasoning,
                tool_calls: self.calls,
                provider_history: None,
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
/// Length of the longest proper prefix of `tag` that ends `s`.
fn tag_prefix_len(s: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&n| s.ends_with(&tag[..n]))
        .unwrap_or(0)
}

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
        unreported: false,
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
        Some("content_filter") => Ok(Finish::ContentFilter),
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

/// Cloud model: a failed or timed-out connect sent no request and may be tried again here.
/// `post_sse` returns TIMEOUT only from connect; after connecting, even silence is STREAM_BROKEN.
/// A cut or silent stream may already be a running, billed upstream generation; the core's single
/// retry layer decides about it, so it is not re-sent from the engine as well (#574).
fn retryable_remote(e: &OxaError) -> bool {
    matches!(e.code, codes::ENGINE_DOWN | codes::TIMEOUT)
}

/// The headers a cloud model gets: the session and the request's `Idempotency-Key`.
fn remote_headers(h: &Hints) -> Vec<(&'static str, String)> {
    let mut out = Vec::with_capacity(3);
    if !h.session.is_empty() {
        out.push(("X-Oxe-Session", h.session.to_string()));
        out.push(("X-Session-Id", h.session.to_string()));
    }
    if !h.idem_key.is_empty() {
        out.push(("Idempotency-Key", h.idem_key.to_string()));
    }
    out
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
    /// Wave D D4: model ids the controller advertised as `remote` in the last info call. Their
    /// requests skip the local X-Oxe-* hints and local-only body fields.
    remote_models: Arc<Mutex<HashSet<String>>>,
    /// The handshake warning of the last [`Engine::info`] (see `handshake.rs`).
    handshake: Arc<std::sync::Mutex<Option<String>>>,
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
            remote_models: Arc::new(Mutex::new(HashSet::new())),
            handshake: Arc::default(),
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

impl HttpEngine {
    /// True when the controller advertised this model as remote in an earlier info call.
    fn is_remote(&self, model: &str) -> bool {
        self.remote_models
            .lock()
            .map(|m| m.contains(model))
            .unwrap_or(false)
    }
}

impl Engine for HttpEngine {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        let (info, cancel, handshake) = fetch_info_full(&self.url, self.api_key.as_deref(), target)?;
        self.cancel_ok.store(cancel, Ordering::SeqCst);
        let key = target.filter(|t| !t.is_empty()).unwrap_or(&info.model);
        let mut remote = self.remote_models.lock().unwrap_or_else(|p| p.into_inner());
        if info.caps.remote {
            remote.insert(key.to_string());
        } else {
            remote.remove(key);
        }
        *self.handshake.lock().unwrap_or_else(|p| p.into_inner()) = handshake;
        Ok(info)
    }
    fn handshake_warning(&self) -> Option<String> {
        self.handshake.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        count_tokens(&self.url, self.api_key.as_deref(), text)
    }
    fn clear_hint(&self, hint: &str) -> Result<(), OxaError> {
        clear_hint(&self.url, self.api_key.as_deref(), hint)
    }
    fn count_prompt(&self, r: &ChatRequest) -> Result<Option<u64>, OxaError> {
        if self.is_remote(r.model) {
            return Ok(None);
        }
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
        let remote = self.is_remote(r.model);
        let mut body = Vec::new();
        build_body_for(r, remote, &mut body);

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
        // A cloud model gets only oxc's own correlation headers (oxc builds the upstream headers
        // itself and never forwards these); the local-only hint headers stay off (#574).
        let mut headers = if remote {
            remote_headers(&hints)
        } else {
            build_headers(&hints)
        };
        if let Some(k) = self.api_key.as_deref().filter(|k| !k.is_empty()) {
            headers.push(("Authorization", format!("Bearer {k}")));
        }
        let level = r.effective_level();
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
                                    failure = Some(if emitted {
                                        broken(format!("{}{}", oxa_proto::CUT_BY_REFUSAL, e.message))
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
                        for e in asm.flush() {
                            on(e);
                        }
                        asm.finish(level)
                    }
                }
            };

            match outcome {
                Ok(turn) => return Ok(turn),
                Err(e) => {
                    let again = if remote {
                        retryable_remote(&e)
                    } else {
                        retryable(&e)
                    };
                    if !began && again && attempt < self.connect_retries {
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
mod retry_tests {
    use super::retryable_remote;
    use oxa_proto::{codes, OxaError};

    #[test]
    fn remote_retries_connect_timeout_but_not_stream_failure() {
        // post_sse returns TIMEOUT only from connect, before sending the request.
        for code in [codes::ENGINE_DOWN, codes::TIMEOUT] {
            assert!(retryable_remote(&OxaError::new(code, "connect failed")), "{code}");
        }
        for code in [codes::STREAM_BROKEN, codes::ABORTED, codes::ENGINE_BUSY] {
            assert!(
                !retryable_remote(&OxaError::new(code, "request failed")),
                "{code}"
            );
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
