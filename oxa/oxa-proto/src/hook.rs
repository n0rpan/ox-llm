use crate::{
    CommandSpec, ExecSpec, KeyId, Mode, OxaError, ThinkingLevel, Tool, ToolOutput, Ui, Value, WireMsg,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Every point at which hooks (extensions, exec hooks, MCP, codemode, subagent) are consulted.
/// One per pi extension event that oxa keeps; [`HookPoint::as_str`] is pi's event name.
///
/// The payload passed to [`Hooks::fire`] is the JSON form of pi's event object for that event
/// (extensions.md), with oxa's names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    ProjectTrust,
    ResourcesDiscover,
    SessionStart,
    SessionShutdown,
    SessionBeforeSwitch,
    SessionBeforeFork,
    SessionBeforeCompact,
    SessionCompact,
    SessionCompactFailed,
    SessionBeforeTree,
    SessionTree,
    SessionInfoChanged,
    Input,
    UserBash,
    BeforeAgentStart,
    AgentStart,
    AgentEnd,
    AgentBeforeSettle,
    AgentSettled,
    TurnStart,
    TurnEnd,
    MessageStart,
    MessageUpdate,
    MessageEnd,
    Context,
    ContextWithSystem,
    BeforeProviderRequest,
    AfterProviderResponse,
    ProviderStreamEvent,
    CacheWarmingDecision,
    ToolCall,
    ToolResult,
    ToolExecutionStart,
    ToolExecutionUpdate,
    ToolExecutionEnd,
    ModelSelect,
    ThinkingLevelSelect,
    UiPromptStart,
    UiPromptEnd,
}

/// How the W-guard treats replies of a hook point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardClass {
    /// May only observe; any reply other than `Continue` is ignored.
    Observer,
    /// May reply with its documented append-safe replies (cancel, block, replace input / args /
    /// result, append a message, summary, trust). Cannot break W1.
    AppendSafe,
    /// May return a message list ([`HookReply::Rebase`]). Accepted silently only if the current
    /// wire is a byte prefix of it; otherwise refused with `guard_refused` unless the extension
    /// declared `rebase: true`, and then logged as a cache-miss cause.
    PrefixChecked,
}

impl HookPoint {
    pub const ALL: [HookPoint; 39] = [
        HookPoint::ProjectTrust,
        HookPoint::ResourcesDiscover,
        HookPoint::SessionStart,
        HookPoint::SessionShutdown,
        HookPoint::SessionBeforeSwitch,
        HookPoint::SessionBeforeFork,
        HookPoint::SessionBeforeCompact,
        HookPoint::SessionCompact,
        HookPoint::SessionCompactFailed,
        HookPoint::SessionBeforeTree,
        HookPoint::SessionTree,
        HookPoint::SessionInfoChanged,
        HookPoint::Input,
        HookPoint::UserBash,
        HookPoint::BeforeAgentStart,
        HookPoint::AgentStart,
        HookPoint::AgentEnd,
        HookPoint::AgentBeforeSettle,
        HookPoint::AgentSettled,
        HookPoint::TurnStart,
        HookPoint::TurnEnd,
        HookPoint::MessageStart,
        HookPoint::MessageUpdate,
        HookPoint::MessageEnd,
        HookPoint::Context,
        HookPoint::ContextWithSystem,
        HookPoint::BeforeProviderRequest,
        HookPoint::AfterProviderResponse,
        HookPoint::ProviderStreamEvent,
        HookPoint::CacheWarmingDecision,
        HookPoint::ToolCall,
        HookPoint::ToolResult,
        HookPoint::ToolExecutionStart,
        HookPoint::ToolExecutionUpdate,
        HookPoint::ToolExecutionEnd,
        HookPoint::ModelSelect,
        HookPoint::ThinkingLevelSelect,
        HookPoint::UiPromptStart,
        HookPoint::UiPromptEnd,
    ];

    /// pi's event name.
    pub fn as_str(self) -> &'static str {
        match self {
            HookPoint::ProjectTrust => "project_trust",
            HookPoint::ResourcesDiscover => "resources_discover",
            HookPoint::SessionStart => "session_start",
            HookPoint::SessionShutdown => "session_shutdown",
            HookPoint::SessionBeforeSwitch => "session_before_switch",
            HookPoint::SessionBeforeFork => "session_before_fork",
            HookPoint::SessionBeforeCompact => "session_before_compact",
            HookPoint::SessionCompact => "session_compact",
            HookPoint::SessionCompactFailed => "session_compact_failed",
            HookPoint::SessionBeforeTree => "session_before_tree",
            HookPoint::SessionTree => "session_tree",
            HookPoint::SessionInfoChanged => "session_info_changed",
            HookPoint::Input => "input",
            HookPoint::UserBash => "user_bash",
            HookPoint::BeforeAgentStart => "before_agent_start",
            HookPoint::AgentStart => "agent_start",
            HookPoint::AgentEnd => "agent_end",
            HookPoint::AgentBeforeSettle => "agent_before_settle",
            HookPoint::AgentSettled => "agent_settled",
            HookPoint::TurnStart => "turn_start",
            HookPoint::TurnEnd => "turn_end",
            HookPoint::MessageStart => "message_start",
            HookPoint::MessageUpdate => "message_update",
            HookPoint::MessageEnd => "message_end",
            HookPoint::Context => "context",
            HookPoint::ContextWithSystem => "context_with_system",
            HookPoint::BeforeProviderRequest => "before_provider_request",
            HookPoint::AfterProviderResponse => "after_provider_response",
            HookPoint::ProviderStreamEvent => "provider_stream_event",
            HookPoint::CacheWarmingDecision => "cache_warming_decision",
            HookPoint::ToolCall => "tool_call",
            HookPoint::ToolResult => "tool_result",
            HookPoint::ToolExecutionStart => "tool_execution_start",
            HookPoint::ToolExecutionUpdate => "tool_execution_update",
            HookPoint::ToolExecutionEnd => "tool_execution_end",
            HookPoint::ModelSelect => "model_select",
            HookPoint::ThinkingLevelSelect => "thinking_level_select",
            HookPoint::UiPromptStart => "ui_prompt_start",
            HookPoint::UiPromptEnd => "ui_prompt_end",
        }
    }

    pub fn parse(s: &str) -> Option<HookPoint> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// The W-guard class of this point (plan sec. 2, "W-guard for extensions and hooks").
    pub fn guard(self) -> GuardClass {
        use HookPoint::*;
        match self {
            Context | ContextWithSystem | BeforeProviderRequest => GuardClass::PrefixChecked,
            ProjectTrust | ResourcesDiscover | SessionBeforeSwitch | SessionBeforeFork
            | SessionBeforeCompact | SessionBeforeTree | Input | UserBash | BeforeAgentStart
            | AgentBeforeSettle | ToolCall | ToolResult | CacheWarmingDecision => GuardClass::AppendSafe,
            _ => GuardClass::Observer,
        }
    }
}

/// A hook's answer.
///
/// **Composition.** The core fires the hook sets of `Deps::hooks` in order. `Continue` moves on.
/// A transforming reply (`ReplaceInput`, `ReplaceArgs`, `ReplaceResult`, `Append`) is applied and
/// the next hook set sees the transformed payload. Any other reply ends the chain.
/// Which replies a point accepts (others are ignored with a warning):
///
/// | Point | Accepted |
/// |---|---|
/// | `ProjectTrust` | `Trust`, `TrustRemembered` |
/// | `Input` | `ReplaceInput`, `Handled` |
/// | `UserBash` | `ReplaceResult`, `Block` |
/// | `BeforeAgentStart` | `Append` (messages appended after the user message) |
/// | `ToolCall` | `Block`, `ReplaceArgs` (before execution) |
/// | `ToolResult` | `ReplaceResult` (once, before the push) |
/// | `SessionBeforeCompact` | `Cancel`, `Summary` (still lands as the W5 head) |
/// | `SessionBeforeTree` | `Cancel`, `Summary` |
/// | `SessionBeforeSwitch`, `SessionBeforeFork` | `Cancel` |
/// | `AgentBeforeSettle` | `Append` (a follow-up user message) |
/// | `CacheWarmingDecision` | `Cancel` (skip the idle prefill) |
/// | `Context`, `ContextWithSystem`, `BeforeProviderRequest` | `Rebase` (append only), `DeclaredRebase` |
#[derive(Debug, Clone, PartialEq)]
pub enum HookReply {
    Continue,
    Cancel(String),
    Block(String),
    ReplaceInput(String),
    /// The input was consumed by the hook; no prompt is sent.
    Handled,
    /// New argument string for the tool call that is about to run. The wire keeps the
    /// assistant's original `args_raw` (W2); only execution sees the replacement.
    ReplaceArgs(String),
    ReplaceResult(ToolOutput),
    Append(Vec<WireMsg>),
    /// The complete message list the hook wants on the wire. See [`GuardClass::PrefixChecked`].
    /// The core accepts it only when the current wire is a prefix of it (an append); any other
    /// list is refused with `guard_refused` and the request goes out unchanged (W1).
    Rebase(Vec<WireMsg>),
    /// A complete message list from a source that declared `rebase: true` (E-INT 3). `origin`
    /// names that source (the extension's path) and is the origin flag of the reply path: only
    /// this variant may change the wire. The core applies it with `rebase_why::EXTENSION`, logs
    /// it (a notice and a `custom` entry `extension_rebase`) and the following cache miss has
    /// the cause `miss_cause::EXTENSION`. A hook set must never build this variant for a source
    /// that did not declare the rebase.
    DeclaredRebase {
        messages: Vec<WireMsg>,
        origin: String,
    },
    Summary {
        text: String,
        details: Value,
    },
    Trust(bool),
    /// As `Trust`, and the decision is saved for the project (pi's `remember: true`).
    TrustRemembered(bool),
}

/// How a hook-injected message is delivered (pi's `deliverAs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Delivered after the current tool batch (steering queue).
    Steer,
    /// Delivered when the agent would otherwise stop (follow-up queue).
    FollowUp,
    /// Kept until the user's next prompt and appended with it.
    NextTurn,
}

/// What an extension (or codemode script, or MCP manager) may ask of the running session.
///
/// **Timing rule.** Queries answer at once. Appends (`SendMessage`, `SendUserMessage`,
/// `AppendEntry`) are queued per [`Delivery`]. Session-shape actions (`Compact`, `NewSession`,
/// `Fork`, `NavigateTree`, `SwitchSession`, `Reload`, `SetActiveTools`, `SetModel`) requested
/// during a run are applied when the run settles: an epoch or rebase never happens mid-run
/// (W1, W3). `Exec`, `ReadFile`, `WriteFile`, `Fetch` and `CallTool` pass the same `Policy` as
/// tools.
#[derive(Debug, Clone, PartialEq)]
pub enum HostAction {
    SendMessage {
        custom_type: String,
        content: String,
        display: bool,
        details: Value,
        deliver: Delivery,
        trigger_turn: bool,
    },
    SendUserMessage {
        text: String,
        deliver: Delivery,
    },
    AppendEntry {
        custom_type: String,
        data: Value,
    },
    SetSessionName(String),
    SetLabel {
        target: String,
        label: Option<String>,
    },
    /// -> array of tool names.
    GetActiveTools,
    /// -> array of `{name, description, source, exposure}`.
    GetAllTools,
    SetActiveTools(Vec<String>),
    SetModel(String),
    SetThinking(ThinkingLevel),
    SetMode(Mode),
    /// -> the RPC `get_state` object.
    GetState,
    /// -> all entries of the session file, pi entry JSON.
    GetEntries,
    /// -> entries of the active branch, root first.
    GetBranch,
    /// -> `{tokens, contextWindow, percent}`.
    ContextUsage,
    Compact {
        instructions: Option<String>,
    },
    NewSession,
    Fork {
        entry: String,
    },
    NavigateTree {
        target: String,
        summarize: bool,
    },
    SwitchSession {
        path: String,
    },
    Reload,
    Abort,
    /// -> bool.
    IsIdle,
    /// Blocks the calling script until the session is idle.
    WaitForIdle,
    Shutdown,
    /// -> `{output, exit, timedOut}`; sandboxed.
    Exec(ExecSpec),
    /// -> string (UTF-8) content.
    ReadFile(PathBuf),
    WriteFile {
        path: PathBuf,
        data: Vec<u8>,
    },
    /// -> `{status, headers, body}`. Needs a `Net` grant.
    Fetch {
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        body: Option<Vec<u8>>,
    },
    /// E11: make the session's own head durable before another session takes the engine's one
    /// slot: one zero-token request over the current wire (H6) with `X-Oxe-Snapshot: keep`
    /// (H4), in the foreground. -> `{parked: bool, promptTokens, snapPos}`; `parked: false`
    /// (no request sent) when the engine does not advertise H6.
    ParkSession,
    /// E11 / EG5: clear the engine state of a finished child session by its session hint
    /// (`<session id>.s<n>`; any other hint is refused). -> `{cleared: bool}`.
    ClearEngineHint(String),
    /// -> `{sessionId, model, remote, contextWindow, maxOutput, compactAt, softTokens, hintClear,
    /// prefillOnly, remainingTokens}`: the limits of the session's model as the session uses
    /// them (E11 child limits). `remainingTokens` is null without a run token budget.
    EngineLimits,
    /// Charge a subagent's uncached prompt + output tokens to the parent's run budget.
    /// -> `null`; a depleted budget requests the same cooperative wrap-up as a deadline.
    AccountRunTokens(u64),
    /// -> `{model, remote}`: whether a model id runs at a cloud provider through oxc (#458:
    /// remote subagent children may run concurrently and on their agent's own model).
    ModelInfo(String),
    /// Run a registered tool through the full loop path (policy, shape). -> `{text, isError}`.
    CallTool {
        name: String,
        args: Value,
    },
    /// -> the session's display name (the last `session_info` on the active branch), or `null`
    /// (pi's `getSessionName`). Answered at once.
    GetSessionName,
    /// -> the thinking level in force (pi's `getThinkingLevel`), e.g. `"high"`.
    GetThinkingLevel,
    /// -> a copy of the effective settings object (defaults < user < trusted project; pi's
    /// `getSettings`).
    GetSettings,
    /// -> the system text of the frozen prefix in force (pi's `ctx.getSystemPrompt`). Read-only:
    /// it changes only at the next epoch (W3).
    GetSystemPrompt,
    /// -> the slash commands invocable through a prompt, pi's `SlashCommandInfo` list:
    /// extension commands, then prompt templates, then skills (`skill:<name>`), each
    /// `{name, description?, source: "extension"|"prompt"|"skill", sourceInfo: {path, source,
    /// scope, origin, baseDir?}}` (pi's `getCommands`).
    GetCommands,
}

/// The session as seen from below `oxa-core`. Implemented by `oxa-core`, handed to every hook
/// set through [`Hooks::attach`].
pub trait Host: Send + Sync {
    fn call(&self, action: HostAction) -> Result<Value, OxaError>;
}

/// A CLI flag registered by an extension (pi's `registerFlag`).
#[derive(Debug, Clone, PartialEq)]
pub struct FlagSpec {
    /// Without the leading dashes.
    pub name: String,
    pub description: String,
    /// `true` = boolean flag, `false` = takes a string value.
    pub boolean: bool,
    pub default: Value,
}

/// A keyboard shortcut registered by an extension (pi's `registerShortcut`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutSpec {
    pub key: KeyId,
    pub description: String,
}

/// What an extension renderer is asked to draw (pi's `registerMessageRenderer` and friends).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RenderKind {
    /// A `custom_message` entry; payload `{customType, content, details, expanded}`.
    Message,
    /// A `custom` entry; payload `{customType, data}`.
    Entry,
    /// A tool call header; payload `{toolName, args}`.
    ToolCall,
    /// A tool result; payload `{toolName, args, result, isError, expanded, partial}`.
    ToolResult,
    /// A Markdown transformer; payload `{messageType, markdown}`; returns one line = the new
    /// Markdown source (joined with `\n`).
    Markdown,
}

/// One source of hooks, tools and commands. `oxa-core` sees only `dyn Hooks`; `oxa-ext` (JS
/// extensions and exec hooks), `oxa-mcp`, `oxa-codemode` and `builtin:subagent` implement it.
///
/// Calls arrive on the session thread, one at a time per hook set. An implementation must
/// return within its own deadline and must never panic across this boundary: a failing handler
/// is reported as `AgentEvent::ExtensionError` and treated as `Continue`.
pub trait Hooks: Send + Sync {
    /// Fire one hook point. See [`HookReply`] for the accepted replies per point.
    fn fire(&self, p: HookPoint, payload: &Value, ui: &mut dyn Ui) -> HookReply;
    /// Tools offered by this source. Read once per prefix epoch (W3); `Direct` tools enter the
    /// frozen tool list, the others never touch the prefix.
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
    fn commands(&self) -> Vec<CommandSpec>;
    fn run_command(&self, name: &str, args: &str, ui: &mut dyn Ui) -> Result<(), OxaError>;
    /// Extra system prompt sections `(name, text)`. Read once per epoch (W3).
    fn prompt_sections(&self) -> Vec<(String, String)>;

    // ---- optional surface ----
    /// A host preset owns the supplied resources: keep them on reload and
    /// rebuild restored prefixes from its current hooks and tool selection.
    fn owns_resources(&self) -> bool {
        false
    }
    /// Whether this preset supports session-local goal controls.
    fn allows_goals(&self) -> bool {
        true
    }
    /// A stable name for logs and error events.
    fn name(&self) -> &str {
        "hooks"
    }
    /// Called once after the session exists and again after every session switch.
    fn attach(&self, _host: Arc<dyn Host>) {}
    fn flags(&self) -> Vec<FlagSpec> {
        Vec::new()
    }
    fn set_flag(&self, _name: &str, _value: &Value) {}
    fn shortcuts(&self) -> Vec<ShortcutSpec> {
        Vec::new()
    }
    fn run_shortcut(&self, _key: &KeyId, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Ok(())
    }
    /// Draw with a registered renderer. `None` = no renderer for this payload; the built-in
    /// view is used.
    fn render(&self, _kind: RenderKind, _payload: &Value, _width: u16) -> Option<Vec<String>> {
        None
    }
    /// Completions offered for the editor text before the cursor (pi's autocomplete providers).
    fn complete(&self, _prefix: &str) -> Vec<String> {
        Vec::new()
    }
    /// Drop all script contexts and child processes (`/reload`, exit). Idempotent.
    fn shutdown(&self) {}
    /// Whether this source has anything to run at `p`. The core skips a source that answers
    /// `false` for that point: no payload is built when no source wants it, which is what keeps
    /// the per-delta points (`MessageUpdate`, `ProviderStreamEvent`, `ToolExecutionUpdate`)
    /// free when nothing subscribed. Must be cheap and must not block; `true` (the default) is
    /// always correct.
    fn wants(&self, _p: HookPoint) -> bool {
        true
    }
    /// `/reload`: drop and re-create what the hook set loaded from code (extension contexts),
    /// loading `extension_paths` again. The core fires `SessionShutdown` before and
    /// `SessionStart` (reason `reload`) after, and attaches a fresh host. Default: nothing.
    fn reload(&self, _extension_paths: &[std::path::PathBuf]) {}
    /// The handler failures recorded since the last call (a throwing or timed-out handler).
    /// The core drains them after each call into the hook set and reports each one as
    /// `AgentEvent::ExtensionError` (pi's `extension_error`).
    fn take_errors(&self) -> Vec<HookError> {
        Vec::new()
    }
    /// Answer one wait condition: the `wait` tool's `until` object (W2). `None` = not this
    /// set's kind; `Ok(None)` = still pending; `Ok(Some(text))` = done, `text` is the probe
    /// evidence; `Err` = the probe failed. The first set that answers `Some` decides.
    /// `budget` is what the wait has left: a probe must not run past it, and may return
    /// `Ok(None)` without work when it is zero.
    fn poll_event(
        &self,
        _kind: &str,
        _args: &Value,
        _budget: std::time::Duration,
    ) -> Option<Result<Option<String>, String>> {
        None
    }
}

/// One failed handler of a hook set ([`Hooks::take_errors`]): pi's `extension_error` fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookError {
    /// The extension file (or `command:<name>` for a command handler).
    pub extension_path: String,
    /// The hook point or event name (`tool_call`, `command`, ...).
    pub event: String,
    pub error: String,
}

/// The default: no hooks, no tools, no commands.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHooks;

impl Hooks for NoHooks {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(
            crate::codes::NOT_FOUND,
            format!("unknown command: /{name}"),
        ))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn wants(&self, _p: HookPoint) -> bool {
        false
    }
}
