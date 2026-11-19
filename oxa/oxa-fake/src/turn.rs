//! Architect-owned (W0-FROZEN): the script and record data shared by both fake engines.
use oxa_proto::{Finish, ToolCall, TurnKind, Usage, WireMsg};

/// A fault injected instead of, or during, a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// Answer with this HTTP status and body instead of a stream.
    Http { status: u16, body: String },
    /// Answer 400 `exceed_context_size_error` (M2).
    Overflow,
    /// Close the connection after this many bytes of SSE body (mid-stream drop). In the
    /// scripted engine: deliver the events of the first `bytes` bytes of text, then fail with
    /// `stream_broken`.
    DropAt { bytes: usize },
    /// Refuse the connection for this request (the scripted engine: `engine_down`).
    Refuse,
    /// Wait this long before sending response headers (server only).
    HoldHeaders { ms: u64 },
    /// Send only `: queued` comments every second for this long before the reply (server only).
    CommentsOnly { ms: u64 },
    /// Close the socket right after `[DONE]` without a clean chunked end (server only; this is
    /// a clean end for the client).
    EarlyClose,
    /// S2 / F10: `count` queued comments in the unified format before the reply, one per
    /// 10 ms: `: queued pos=<n> wait_s=<i>` (the controller; positions count down to 1) when
    /// `pos`, else `: queued wait_s=<i>` (the engine). The scripted engine emits the same
    /// `StreamEvent::Queued`s.
    Queued { count: u32, pos: bool },
    /// F3: an error after early headers. `200` + SSE head, `queued` controller comments, then
    /// the error object `{"message","type","code":status}` and the end of the body without
    /// `[DONE]`. `event`: the controller's form (`event: error` + `data:`); else the engine's own
    /// plain `data: {"error":..}`. `error_type` defaults per status (`exceed_context_size_error`
    /// makes it an overflow).
    StreamError {
        queued: u32,
        status: u16,
        error_type: String,
        event: bool,
    },
    /// F3 / S2: the controller's queue-full reply: 503, type `local_backend_busy`, code
    /// `queue_full`, with `Retry-After: <s>` when `Some`.
    Busy { retry_after: Option<u32> },
    /// H4: the reply carries `timings.snap_pos: null` and this `snap_reason` (only when the
    /// request sent `X-Oxe-Snapshot: keep`).
    SnapNull { reason: String },
    /// Mid-stream stall (server only): send `bytes` bytes of the SSE body, then nothing at all
    /// (no data, no comment) for `ms`, then the rest of the reply. A client idle limit below
    /// `ms` sees a silent stream.
    Stall { bytes: usize, ms: u64 },
    /// Serve cold (both engines): the engine holds nothing for the session when this request
    /// arrives (a server without a prefix cache, an evicted or restarted state), so it reports
    /// `cached: 0` and holds only this request's prompt + reply afterwards.
    Cold,
}

/// One scripted reply. A request with `max_tokens > 0` consumes the next turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptTurn {
    pub reasoning: Option<String>,
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub finish: Finish,
    /// `None` = canonical reply bytes + 1 (the cache model of the crate docs).
    pub completion_tokens: Option<u32>,
    /// Delta size in bytes for streaming (0 = one delta per field). Splits never cut a UTF-8
    /// sequence.
    pub chunk: usize,
    pub fault: Option<Fault>,
}

impl ScriptTurn {
    /// A plain text reply that stops.
    pub fn text(t: &str) -> ScriptTurn {
        ScriptTurn {
            reasoning: None,
            text: Some(t.to_string()),
            tool_calls: Vec::new(),
            finish: Finish::Stop,
            completion_tokens: None,
            chunk: 0,
            fault: None,
        }
    }
    /// A reply that calls one tool. `args_raw` is sent exactly as given.
    pub fn tool(id: &str, name: &str, args_raw: &str) -> ScriptTurn {
        ScriptTurn {
            reasoning: None,
            text: None,
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                args_raw: args_raw.to_string(),
            }],
            finish: Finish::ToolCalls,
            completion_tokens: None,
            chunk: 0,
            fault: None,
        }
    }
    pub fn fault(f: Fault) -> ScriptTurn {
        ScriptTurn {
            fault: Some(f),
            ..ScriptTurn::text("")
        }
    }
    /// The assistant message this turn produces (W2: exactly these strings).
    pub fn message(&self) -> WireMsg {
        WireMsg {
            images: Vec::new(),
            role: oxa_proto::Role::Assistant,
            content: self.text.clone(),
            reasoning: self.reasoning.clone(),
            tool_calls: self.tool_calls.clone(),
            provider_history: None,
            tool_call_id: None,
        }
    }
}

/// One request as seen by a fake engine, plus what it answered.
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    /// 0-based arrival index.
    pub seq: usize,
    /// The system message content.
    pub system: String,
    /// The `tools` array as received (`[]` when absent).
    pub tools_json: String,
    /// The messages after the system message.
    pub messages: Vec<WireMsg>,
    pub model: String,
    pub max_tokens: u32,
    /// `chat_template_kwargs.enable_thinking`.
    pub thinking: bool,
    /// Hint values (`""` / `false` when the header is absent).
    pub session: String,
    pub idem_key: String,
    pub kind: String,
    pub background: bool,
    pub snapshot_keep: bool,
    /// `chat_template_kwargs.preserve_thinking` as sent (`None` = the key is absent; F8).
    pub preserve_thinking: Option<bool>,
    /// The `temperature` sent (`0` = greedy, W6).
    pub temperature: f64,
    /// Request headers, names lowercased (empty for the scripted engine).
    pub headers: Vec<(String, String)>,
    /// The raw body (empty for the scripted engine).
    pub body: Vec<u8>,
    /// The reply generated, when one was completed.
    pub reply: Option<WireMsg>,
    pub usage: Option<Usage>,
}

/// B6: the history as the chat template renders it. With `drop_reasoning` (a Qwen template with
/// `preserve_thinking: false`), the assistant turns at or before the last user message lose
/// their reasoning; the turns after it (a tool loop in progress) keep it. Without, the messages
/// as sent (the template default: reasoning kept when the client returns it, W2).
pub fn render_history(msgs: &[WireMsg], drop_reasoning: bool) -> Vec<WireMsg> {
    let mut out = msgs.to_vec();
    if !drop_reasoning {
        return out;
    }
    let last_user = msgs.iter().rposition(|m| m.role == oxa_proto::Role::User);
    if let Some(last_user) = last_user {
        for m in out[..last_user].iter_mut() {
            if m.role == oxa_proto::Role::Assistant {
                m.reasoning = None;
            }
        }
    }
    out
}

/// The bytes the fake's cache model counts: system text, `\0`, tools JSON, `\0`, then each
/// message's canonical JSON followed by `\n`. Appending messages appends bytes.
pub fn cache_bytes(system: &str, tools_json: &str, msgs: &[WireMsg]) -> Vec<u8> {
    let mut v = Vec::with_capacity(system.len() + tools_json.len() + 2);
    v.extend_from_slice(system.as_bytes());
    v.push(0);
    v.extend_from_slice(tools_json.as_bytes());
    v.push(0);
    for m in msgs {
        m.write_json(&mut v);
        v.push(b'\n');
    }
    v
}

/// The anchor of a prompt in the cache model: the frozen prefix bytes (system text, `\0`, tools
/// JSON, `\0`). `None` when the bytes are not a whole anchor.
pub(crate) fn anchor_len(prompt: &[u8]) -> Option<usize> {
    let first = prompt.iter().position(|&b| b == 0)?;
    let second = prompt[first + 1..].iter().position(|&b| b == 0)?;
    Some(first + 1 + second + 1)
}

/// `cached` of a request in the cache model: the common prefix with what the engine holds for
/// `session`, and at least the anchor when ANY session holds the same anchor (the engine's prefix
/// cache shares the system prompt + tools across sessions: a fork, a new session, a child with
/// the same prefix). `cold` (the `cold` fault): nothing at all.
pub(crate) fn cached_of(cache: &[(String, Vec<u8>)], session: &str, prompt: &[u8], cold: bool) -> usize {
    if cold {
        return 0;
    }
    let own = cache
        .iter()
        .find(|(s, _)| s == session)
        .map(|(_, held)| held.iter().zip(prompt).take_while(|(a, b)| a == b).count())
        .unwrap_or(0);
    let shared = anchor_len(prompt)
        .filter(|&n| {
            cache
                .iter()
                .any(|(_, held)| held.len() >= n && held[..n] == prompt[..n])
        })
        .unwrap_or(0);
    own.max(shared)
}

/// `TurnKind` from its header string (`chat` when unknown).
pub fn kind_from_str(s: &str) -> TurnKind {
    match s {
        "compaction" => TurnKind::Compaction,
        "branch_summary" => TurnKind::BranchSummary,
        "prefill" => TurnKind::Prefill,
        "subagent" => TurnKind::Subagent,
        _ => TurnKind::Chat,
    }
}
