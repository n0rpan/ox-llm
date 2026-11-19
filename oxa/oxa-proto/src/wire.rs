use std::path::PathBuf;

/// Chat role on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            _ => return None,
        })
    }
}

/// One tool call of an assistant message.
///
/// **W2:** `id` is the server's id and `args_raw` is the argument string exactly as streamed
/// (the concatenation of the deltas). It is never re-serialised, trimmed or repaired; parsing
/// it for execution happens on a copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args_raw: String,
}

/// One message of the wire (`messages` of `/v1/chat/completions`).
///
/// The system message is *not* a `WireMsg` in a request: it comes from the
/// [`FrozenPrefix`](crate::FrozenPrefix). `Role::System` exists for the rebased head notation
/// `[system, user:<summary>, tail]` and for imports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireMsg {
    pub role: Role,
    pub content: Option<String>,
    /// `reasoning_content` on the wire. Assistant only.
    pub reasoning: Option<String>,
    /// Assistant only; empty otherwise.
    pub tool_calls: Vec<ToolCall>,
    /// Tool role only: the id of the call this message answers.
    pub tool_call_id: Option<String>,
    /// Private, bounded replay material for a completed assistant message only.
    /// Omitted by `write_json`; the gateway validates its scope before use.
    pub provider_history: Option<crate::ProviderHistory>,
}

impl WireMsg {
    pub fn system(text: impl Into<String>) -> Self {
        Self::plain(Role::System, text.into())
    }
    pub fn user(text: impl Into<String>) -> Self {
        Self::plain(Role::User, text.into())
    }
    /// An assistant message with text only. Replies from the engine are built field by field
    /// from the stream instead (W2).
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::plain(Role::Assistant, text.into())
    }
    pub fn tool_result(tool_call_id: impl Into<String>, text: impl Into<String>) -> Self {
        WireMsg {
            role: Role::Tool,
            content: Some(text.into()),
            reasoning: None,
            tool_calls: Vec::new(),
            provider_history: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
    fn plain(role: Role, text: String) -> Self {
        WireMsg {
            role,
            content: Some(text),
            reasoning: None,
            tool_calls: Vec::new(),
            provider_history: None,
            tool_call_id: None,
        }
    }

    /// Append the canonical JSON bytes of this message to `out`.
    ///
    /// This is the only serialiser of a wire message in the workspace; W1 and W2 are byte
    /// properties of its output. Fixed key order, no whitespace:
    ///
    /// * system / user: `{"role":R,"content":C}`
    /// * assistant: `{"role":"assistant","content":C|null[,"reasoning_content":S][,"tool_calls":[T,..]]}`
    ///   with `T` = `{"id":I,"type":"function","function":{"name":N,"arguments":A}}`; `A` is
    ///   `args_raw` as a JSON string. `reasoning_content` is present only when `reasoning` is
    ///   `Some` (also when it is the empty string); `tool_calls` only when non-empty.
    /// * tool: `{"role":"tool","tool_call_id":I,"content":C}`
    ///
    /// `content: None` is written as `null`. Strings are escaped by `serde_json` (non-ASCII is
    /// written raw as UTF-8).
    pub fn write_json(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"{\"role\":\"");
        out.extend_from_slice(self.role.as_str().as_bytes());
        out.push(b'"');
        if self.role == Role::Tool {
            out.extend_from_slice(b",\"tool_call_id\":");
            write_json_str(self.tool_call_id.as_deref().unwrap_or(""), out);
        }
        out.extend_from_slice(b",\"content\":");
        match &self.content {
            Some(c) => write_json_str(c, out),
            None => out.extend_from_slice(b"null"),
        }
        if self.role == Role::Assistant {
            if let Some(r) = &self.reasoning {
                out.extend_from_slice(b",\"reasoning_content\":");
                write_json_str(r, out);
            }
            if !self.tool_calls.is_empty() {
                out.extend_from_slice(b",\"tool_calls\":[");
                for (i, t) in self.tool_calls.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(b"{\"id\":");
                    write_json_str(&t.id, out);
                    out.extend_from_slice(b",\"type\":\"function\",\"function\":{\"name\":");
                    write_json_str(&t.name, out);
                    out.extend_from_slice(b",\"arguments\":");
                    write_json_str(&t.args_raw, out);
                    out.extend_from_slice(b"}}");
                }
                out.push(b']');
            }
        }
        out.push(b'}');
    }

    /// Canonical bytes as a new vector.
    pub fn to_json(&self) -> Vec<u8> {
        let mut v = Vec::new();
        self.write_json(&mut v);
        v
    }
}

/// Append `s` as a JSON string literal (the workspace's one string escaper for wire bytes).
pub fn write_json_str(s: &str, out: &mut Vec<u8>) {
    // serde_json's escaping is stable: `"` `\` and control characters only.
    serde_json::to_writer(&mut *out, s).expect("writing a string to a Vec cannot fail");
}

/// Append `msgs` as a JSON array of canonical messages (no surrounding key).
pub fn write_wire_json(msgs: &[WireMsg], out: &mut Vec<u8>) {
    out.push(b'[');
    for (i, m) in msgs.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        m.write_json(out);
    }
    out.push(b']');
}

/// `true` when `longer` starts with the unchanged `shorter` messages (W1 and
/// W-guard), including private replay authority that visible JSON cannot replace.
pub fn wire_is_prefix(shorter: &[WireMsg], longer: &[WireMsg]) -> bool {
    shorter.len() <= longer.len() && shorter.iter().zip(longer).all(|(a, b)| a == b)
}

/// A part of what the user typed or attached. Text parts are joined into the user message;
/// an image is stored in the session and shown, and the wire gets [`image_placeholder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    Text(String),
    /// Provenance, not content: the environment note oxa appended to this user message's wire
    /// text (`oxa_core::envnote`), byte for byte. Recorded only when oxa appended one, so the
    /// typed text is the wire text minus exactly this suffix; a user message without it is shown
    /// and refilled as its wire text. Never sent, never shown.
    EnvNote(String),
    /// Never serialised to the wire today (the engine is text-only).
    Image {
        path: PathBuf,
        mime: String,
        sha256: [u8; 32],
        w: u32,
        h: u32,
    },
}

/// The one fixed wire line for an image: `[image <name> <w>x<h>: not visible to this model]`.
/// `read` on an image file returns the same line.
pub fn image_placeholder(name: &str, w: u32, h: u32) -> String {
    format!("[image {name} {w}x{h}: not visible to this model]")
}

/// pi's thinking levels. On our engine `Off` = `enable_thinking: false`, every other level =
/// `true` (W6); budgets are ignored with a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }
    pub fn parse(s: &str) -> Option<ThinkingLevel> {
        Self::ALL.into_iter().find(|l| l.as_str() == s)
    }
    /// The W6 `enable_thinking` value.
    pub fn enabled(self) -> bool {
        self != ThinkingLevel::Off
    }
}

/// Token usage of one request as reported by the engine (exact, M2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// `usage.prompt_tokens`.
    pub prompt: u32,
    /// `usage.completion_tokens`.
    pub completion: u32,
    /// `usage.prompt_tokens_details.cached_tokens` (0 when not reported, see `cached_missing`).
    pub cached: u32,
    /// B6: the server did not report `cached_tokens` (`cached` is then 0 and means nothing):
    /// no cache miss is derived from this reply. `false` for every oxe engine.
    pub cached_missing: bool,
    /// The provider finished the reply without reporting token usage.
    pub unreported: bool,
}

impl Usage {
    pub fn unreported() -> Self {
        Self {
            cached_missing: true,
            unreported: true,
            ..Self::default()
        }
    }

    pub fn reported(self) -> Option<Self> {
        (!self.unreported).then_some(self)
    }
}

/// Why the engine stopped. An aborted or failed stream has no `Finish`: it is an `Err`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    Stop,
    ToolCalls,
    Length,
    /// The model declined (a refusal); the streamed text is kept, no retry.
    ContentFilter,
}

/// `timings.*` of the engine's final chunk.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Timings {
    pub cache_n: u32,
    pub prompt_n: u32,
    pub prompt_ms: f64,
    pub predicted_n: u32,
    pub predicted_ms: f64,
    /// H4: token position of the checkpoint the engine kept, when it reports one. `None` with
    /// `snap_asked` = the engine kept none (`timings.snap_reason`).
    pub snap_pos: Option<u32>,
    /// The reply carried the `snap_pos` key at all (a number or null): the engine knows H4.
    pub snap_asked: bool,
}

/// One increment of a streaming reply, in arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// A `: prefill <done>/<total> <secs>s` SSE comment (M2). `secs` = the engine's own count
    /// when the line carries one, else seconds since the request began.
    Prefill { done: u32, total: u32, secs: u32 },
    /// A `: queued [pos=<n>] wait_s=<s>` SSE comment (S2 / F10): the request waits for the one
    /// inference slot. `pos` = place in the controller's queue (1 = next), `None` on the
    /// engine's own line. Liveness, never reply data.
    Queued { pos: Option<u32>, wait_s: u32 },
    /// A `reasoning_content` delta.
    Reasoning(String),
    /// A `content` delta.
    Text(String),
    /// A tool-call delta. `id` and `name` arrive once per `index`; `delta` extends `args_raw`.
    ToolArgs {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        delta: String,
    },
}

/// A complete, successful reply. Only a value of this type may be pushed to the wire (W2).
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantTurn {
    pub msg: WireMsg,
    pub finish: Finish,
    pub usage: Usage,
    pub timings: Option<Timings>,
    pub model: String,
    pub level: ThinkingLevel,
}

/// H5 `X-Oxe-Turn-Kind`: a free ledger label; it never changes generated bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnKind {
    Chat,
    Compaction,
    BranchSummary,
    Prefill,
    Subagent,
}

impl TurnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnKind::Chat => "chat",
            TurnKind::Compaction => "compaction",
            TurnKind::BranchSummary => "branch_summary",
            TurnKind::Prefill => "prefill",
            TurnKind::Subagent => "subagent",
        }
    }
}

/// The frozen prefix of one epoch (W3): the system prompt sections and the tool list.
///
/// Built once by `oxa_core::build_prefix`, stored as a session entry
/// ([`EntryBody::Prefix`](crate::EntryBody::Prefix)) and replayed byte for byte on resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenPrefix {
    /// `(name, text)` in the fixed order of [`FrozenPrefix::SECTION_ORDER`]. Extension and hook
    /// sections come between `mcp_servers` and `append`, sorted by name. Empty sections are
    /// omitted from the vector.
    pub sections: Vec<(String, String)>,
    /// The `tools` array of the request as canonical JSON bytes (sorted by tool name).
    pub tools_json: String,
    /// SHA-256 of `system_text()` + `\0` + `tools_json`.
    pub sha256: [u8; 32],
}

impl FrozenPrefix {
    /// Section names in wire order. `cwd` is always last (W3).
    ///
    /// O-152: `identity` follows `preamble`, and `skills` comes before `context`, so the leading
    /// [`FrozenPrefix::ANCHOR_SECTIONS`] (with the tools) are the same bytes in every project:
    /// the warm-start anchor. Project-specific text (`context`, `cwd`) comes after it.
    pub const SECTION_ORDER: [&'static str; 7] = [
        "preamble",
        "identity",
        "skills",
        "context",
        "mcp_servers",
        "append",
        "cwd",
    ];

    /// O-152: the sections of the warm-start anchor, a leading run of [`SECTION_ORDER`]. They
    /// depend on the install (binary, identity file, user-level skills), never on the working
    /// directory, the date or the session.
    ///
    /// [`SECTION_ORDER`]: FrozenPrefix::SECTION_ORDER
    pub const ANCHOR_SECTIONS: [&'static str; 3] = ["preamble", "identity", "skills"];

    /// O-152: the system text of the anchor: the leading sections of [`FrozenPrefix::sections`]
    /// that are in [`FrozenPrefix::ANCHOR_SECTIONS`], joined like [`FrozenPrefix::system_text`]
    /// (so it is a byte prefix of it).
    pub fn anchor_text(&self) -> String {
        let mut s = String::new();
        for (i, (_, text)) in self
            .sections
            .iter()
            .take_while(|(n, _)| Self::ANCHOR_SECTIONS.contains(&n.as_str()))
            .enumerate()
        {
            if i > 0 {
                s.push_str("\n\n");
            }
            s.push_str(text);
        }
        s
    }

    /// The system message text: the section texts joined by one blank line.
    pub fn system_text(&self) -> String {
        let mut s = String::new();
        for (i, (_, text)) in self.sections.iter().enumerate() {
            if i > 0 {
                s.push_str("\n\n");
            }
            s.push_str(text);
        }
        s
    }
    /// The bytes that [`FrozenPrefix::sha256`] is the hash of.
    pub fn hash_input(&self) -> Vec<u8> {
        let mut v = self.system_text().into_bytes();
        v.push(0);
        v.extend_from_slice(self.tools_json.as_bytes());
        v
    }
}

/// The only causes that may rebase the wire (W1). `Transcript::rebase` takes one of these.
pub mod rebase_why {
    pub const COMPACTION: &str = "compaction";
    pub const LEAF_MOVE: &str = "leaf_move";
    pub const CONTEXT_EDIT: &str = "context_edit";
    pub const EPOCH: &str = "epoch";
    pub const EXTENSION: &str = "extension";
    pub const ALL: [&str; 5] = [COMPACTION, LEAF_MOVE, CONTEXT_EDIT, EPOCH, EXTENSION];
}

/// Causes reported with a cache miss (`AgentEvent::CacheMiss`). `UNKNOWN` must never occur in
/// the hardware gate (plan sec. 6.6).
pub mod miss_cause {
    pub const COMPACTION: &str = "compaction";
    pub const LEAF_MOVE: &str = "leaf_move";
    pub const CONTEXT_EDIT: &str = "context_edit";
    pub const EPOCH: &str = "epoch";
    pub const EXTENSION: &str = "extension";
    /// First request of a process on a session the engine no longer holds.
    pub const COLD_START: &str = "cold_start";
    /// The engine evicted or lost the session's state.
    pub const ENGINE_EVICTED: &str = "engine_evicted";
    pub const UNKNOWN: &str = "unknown";
}

/// Reasons a reply is shown but never sent (W2), stored in [`EntryBody::Shown`](crate::EntryBody::Shown).
pub mod shown_why {
    pub const ABORTED: &str = "aborted";
    pub const FAILED: &str = "failed";
    /// The stream was cut after the first data byte.
    pub const CUT: &str = "cut";
    /// A notice for the user (epoch cost, rebase notice, warning).
    pub const NOTICE: &str = "notice";
    pub const ALL: [&str; 4] = [ABORTED, FAILED, CUT, NOTICE];
    /// Map a stored string back to the static constant (`NOTICE` when unknown).
    pub fn intern(s: &str) -> &'static str {
        ALL.into_iter().find(|w| *w == s).unwrap_or(NOTICE)
    }
}
