use crate::{Entry, Finish, Level, Mode, ThinkingLevel, ToolCall, ToolOutput, Usage, Value, WireMsg};

/// pi's `stopReason` of a message record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Pending,
    Stop,
    ToolUse,
    Length,
    Aborted,
    Error,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Pending => "pending",
            StopReason::Stop => "stop",
            StopReason::ToolUse => "toolUse",
            StopReason::Length => "length",
            StopReason::Aborted => "aborted",
            StopReason::Error => "error",
        }
    }
}

impl From<Finish> for StopReason {
    fn from(f: Finish) -> Self {
        match f {
            Finish::Stop => StopReason::Stop,
            Finish::ToolCalls => StopReason::ToolUse,
            Finish::Length => StopReason::Length,
        }
    }
}

/// A message as carried by events. `oxa-rpc` maps it to pi's message JSON (json.md); the
/// in-process SDK reads the fields directly.
#[derive(Debug, Clone, PartialEq)]
pub struct EventMessage {
    pub msg: WireMsg,
    pub ts_ms: u64,
    /// Assistant only; `Pending` on `MessageStart`.
    pub stop: Option<StopReason>,
    /// Assistant only: cumulative usage of the reply.
    pub usage: Option<Usage>,
    /// Assistant only.
    pub model: Option<String>,
    /// Tool results only: the tool's name and error flag.
    pub tool_name: Option<String>,
    pub is_error: bool,
    /// Set when `stop` is `Aborted` or `Error`.
    pub error_message: Option<String>,
}

/// pi's `assistantMessageEvent` (delta-only form; json.md "Reconstruct streaming messages").
/// `index` is pi's `contentIndex`: thinking block (if any) first, then text, then tool calls in
/// stream order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageDelta {
    TextStart { index: usize },
    TextDelta { index: usize, delta: String },
    TextEnd { index: usize, content: String },
    ThinkingStart { index: usize },
    ThinkingDelta { index: usize, delta: String },
    ThinkingEnd { index: usize, content: String },
    ToolcallStart { index: usize, id: String, name: String },
    ToolcallDelta { index: usize, delta: String },
    ToolcallEnd { index: usize, call: ToolCall },
}

/// Why a compaction started (pi's `reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactReason {
    Manual,
    Threshold,
    Overflow,
}

impl CompactReason {
    pub fn as_str(self) -> &'static str {
        match self {
            CompactReason::Manual => "manual",
            CompactReason::Threshold => "threshold",
            CompactReason::Overflow => "overflow",
        }
    }
}

/// pi's `CompactionResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u32,
    pub estimated_tokens_after: u32,
    pub usage: Option<Usage>,
    pub details: Value,
}

/// Everything a front end (TUI, JSON, RPC, SDK user) can observe. One variant per pi event, with
/// record names 1:1 with pi's json.md ([`AgentEvent::name`]); four are ours.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<EventMessage>,
        will_retry: bool,
    },
    AgentSettled {
        aborted: bool,
    },
    TurnStart,
    TurnEnd {
        message: EventMessage,
        tool_results: Vec<EventMessage>,
    },
    MessageStart {
        message: EventMessage,
    },
    MessageUpdate {
        usage: Usage,
        delta: MessageDelta,
    },
    MessageEnd {
        message: EventMessage,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial: String,
    },
    /// `duration_ms` is for display only and never reaches the wire (W4).
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: ToolOutput,
        duration_ms: Option<u64>,
    },
    CompactionStart {
        reason: CompactReason,
    },
    CompactionEnd {
        reason: CompactReason,
        result: Option<CompactionResult>,
        aborted: bool,
        will_retry: bool,
        error_message: Option<String>,
    },
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
    SummarizationRetryScheduled {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    /// `source` is `"compaction"` or `"branchSummary"`; `reason` is absent for a branch summary.
    SummarizationRetryAttemptStart {
        source: &'static str,
        reason: Option<CompactReason>,
    },
    SummarizationRetryFinished,
    /// Both vectors are the complete current queues.
    QueueUpdate {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    EntryAppended {
        entry: Entry,
    },
    ModelSelect {
        model: String,
        previous: Option<String>,
    },
    ThinkingLevelChanged {
        level: ThinkingLevel,
    },
    SessionInfoChanged {
        name: Option<String>,
    },
    /// RPC `bash` and `!cmd` output chunks.
    BashExecutionUpdate {
        id: Option<String>,
        delta: String,
    },
    ExtensionError {
        extension_path: String,
        event: String,
        error: String,
    },
    // ---- ours ----
    /// Engine prefill progress (`: prefill` comments).
    Prefill {
        done: u32,
        total: u32,
        secs: u32,
    },
    /// The request waits for the engine's one slot (`: queued`, S2 / ENG-003). Not an error;
    /// repeated while waiting. `pos` = place in the queue when the controller reports it.
    EngineQueue {
        pos: Option<u32>,
        wait_s: u32,
    },
    /// A new prefix epoch began (W3). `reprefill_tokens` is its printed cost.
    PrefixEpoch {
        sha256: [u8; 32],
        reason: String,
        reprefill_tokens: u32,
    },
    /// `cached < expected` after a reply. `cause` is one of [`crate::miss_cause`].
    CacheMiss {
        expected: u32,
        cached: u32,
        cause: &'static str,
    },
    ModeChanged {
        mode: Mode,
    },
    /// A one-line notice for the user (warnings from settings, epoch cost, guard refusals).
    Notice {
        level: Level,
        text: String,
    },
    /// T2: a reply without tool calls held a tool call the engine could not parse; one repair
    /// message was appended (`attempt` of at most 2 in a row) or, with `stopped`, the run
    /// stopped at the cap. `cause` is one of [`crate::repair_cause`]; `total` is the session's
    /// `tool_repairs` counter after this event.
    ToolRepair {
        attempt: u32,
        cause: &'static str,
        total: u32,
        stopped: bool,
    },
}

impl AgentEvent {
    /// The record `type` string (pi's names for pi's events).
    pub fn name(&self) -> &'static str {
        match self {
            AgentEvent::AgentStart => "agent_start",
            AgentEvent::AgentEnd { .. } => "agent_end",
            AgentEvent::AgentSettled { .. } => "agent_settled",
            AgentEvent::TurnStart => "turn_start",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate { .. } => "message_update",
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
            AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
            AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
            AgentEvent::CompactionStart { .. } => "compaction_start",
            AgentEvent::CompactionEnd { .. } => "compaction_end",
            AgentEvent::AutoRetryStart { .. } => "auto_retry_start",
            AgentEvent::AutoRetryEnd { .. } => "auto_retry_end",
            AgentEvent::SummarizationRetryScheduled { .. } => "summarization_retry_scheduled",
            AgentEvent::SummarizationRetryAttemptStart { .. } => "summarization_retry_attempt_start",
            AgentEvent::SummarizationRetryFinished => "summarization_retry_finished",
            AgentEvent::QueueUpdate { .. } => "queue_update",
            AgentEvent::EntryAppended { .. } => "entry_appended",
            AgentEvent::ModelSelect { .. } => "model_select",
            AgentEvent::ThinkingLevelChanged { .. } => "thinking_level_changed",
            AgentEvent::SessionInfoChanged { .. } => "session_info_changed",
            AgentEvent::BashExecutionUpdate { .. } => "bash_execution_update",
            AgentEvent::ExtensionError { .. } => "extension_error",
            AgentEvent::Prefill { .. } => "prefill",
            AgentEvent::EngineQueue { .. } => "engine_queue",
            AgentEvent::PrefixEpoch { .. } => "prefix_epoch",
            AgentEvent::CacheMiss { .. } => "cache_miss",
            AgentEvent::ModeChanged { .. } => "mode_changed",
            AgentEvent::Notice { .. } => "notice",
            AgentEvent::ToolRepair { .. } => "tool_repair",
        }
    }
}

/// Receives events on the thread that runs the session. Implementations must not block for
/// long: a slow sink slows the agent loop.
pub trait EventSink {
    fn emit(&mut self, e: &AgentEvent);
}

/// A sink that drops everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _e: &AgentEvent) {}
}

/// A sink that collects events (tests, `agent_end.messages`).
#[derive(Debug, Default, Clone)]
pub struct VecSink(pub Vec<AgentEvent>);

impl EventSink for VecSink {
    fn emit(&mut self, e: &AgentEvent) {
        self.0.push(e.clone());
    }
}
