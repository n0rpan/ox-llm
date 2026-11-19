//! Architect-owned: the shared view model, commands and UI requests (pure data, complete).
//! Not a task file. D1, D2, D4, D5, D6, D10 and D11 all meet here; nothing in this file may
//! change without the architect.
use oxa_config::{Dirs, Keymap, Settings};
use oxa_core::Session;
use oxa_mcp::McpManager;
use oxa_proto::{
    Access, Answer, CustomOpts, DangerAnswer, Hooks, Level, Mode, OxaError, Part, Placement, StopReason,
    ThinkingLevel, ToolOutput, UiComponent, Value,
};
use oxa_session::{SessionRef, SessionStore};
use oxa_tui::TermCaps;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

// ---------------------------------------------------------------------------------------------
// Transcript blocks
// ---------------------------------------------------------------------------------------------

/// Execution state of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    /// The call is still streaming (arguments incomplete).
    Streaming,
    /// Arguments complete, not started (waiting for permission or for its turn).
    Pending,
    Running,
    Done,
    Error,
}

/// One tool call and its result (pi's `ToolExecutionComponent` data).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolBlock {
    pub call_id: String,
    pub name: String,
    /// Parsed arguments; `Value::Null` while `args_raw` is not yet valid JSON.
    pub args: Value,
    /// The argument string as streamed so far.
    pub args_raw: String,
    pub state: ToolState,
    /// Latest partial output (live bash output).
    pub partial: String,
    /// Final result. `result.ui` carries the display extras (diff, exit code).
    pub result: Option<ToolOutput>,
    pub duration_ms: Option<u64>,
    pub expanded: bool,
}

/// An attached image as shown in a user block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub path: PathBuf,
    pub mime: String,
    pub w: u32,
    pub h: u32,
}

/// One block of the transcript, in display order.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    User {
        text: String,
        images: Vec<ImageRef>,
    },
    Assistant {
        text: String,
        thinking: String,
        /// `true` until `message_end`.
        streaming: bool,
        stop: Option<StopReason>,
        error: Option<String>,
        model: Option<String>,
    },
    Tool(ToolBlock),
    Compaction {
        summary: String,
        tokens_before: u32,
        /// Estimated context tokens after the compaction, when known (live `/compact` and
        /// auto-compaction report it; entries resumed from disk predate the estimate).
        tokens_after: Option<u32>,
        expanded: bool,
    },
    BranchSummary {
        summary: String,
        expanded: bool,
    },
    /// An extension `custom_message` (only when `display`).
    Custom {
        custom_type: String,
        content: String,
        details: Value,
        expanded: bool,
    },
    /// A `!cmd` / `!!cmd` execution.
    Bash {
        command: String,
        output: String,
        exit: Option<i32>,
        excluded: bool,
        running: bool,
        expanded: bool,
    },
    /// A reply that was shown but never sent (W2), `why` = `oxa_proto::shown_why`.
    Shown {
        text: String,
        why: String,
    },
    /// A one-line notice (warnings, epoch cost, cache miss, mode change).
    Notice {
        level: Level,
        text: String,
    },
}

impl Block {
    /// `true` when the block can no longer change (regular mode commits it to scrollback).
    pub fn finished(&self) -> bool {
        match self {
            Block::Assistant { streaming, .. } => !*streaming,
            Block::Tool(t) => matches!(t.state, ToolState::Done | ToolState::Error),
            Block::Bash { running, .. } => !*running,
            _ => true,
        }
    }
    pub fn is_user(&self) -> bool {
        matches!(self, Block::User { .. })
    }
}

// ---------------------------------------------------------------------------------------------
// Status (footer, header, title)
// ---------------------------------------------------------------------------------------------

/// What the agent is doing, for the loader line and the title.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Activity {
    #[default]
    Idle,
    /// Waiting for the first byte; `Some` while the engine reports prefill progress.
    Prefill {
        done: u32,
        total: u32,
        secs: u32,
    },
    Thinking,
    Streaming,
    /// A tool runs; carries the tool name.
    Tool(String),
    Compacting,
    /// Waiting to retry; attempt and delay.
    Retrying {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
    /// A dialog waits for the user.
    Blocked,
    /// The request waits for the engine's one slot (S2, `AgentEvent::EngineQueue`): place in
    /// the queue when the controller reports it, else the seconds waited. Not an error; ended
    /// by the first prefill or data event, the end of the request or an abort.
    Queued {
        pos: Option<u32>,
        wait_s: u32,
    },
}

impl StatusModel {
    /// The thinking text for the status line. The request path only sends a boolean
    /// `enable_thinking` and oxc maps it to the effort: `true` -> `high`, `false` -> `none`
    /// (adaptive `low`). Claude 5.5 cannot disable thinking, so `off` shows as `adaptive • low`
    /// and every other level as `adaptive • high`. Other models show the level name.
    pub fn thinking_label(&self) -> Option<String> {
        self.thinking_label_with("•")
    }

    /// [`Self::thinking_label`] with the separator between `adaptive` and the effort supplied by
    /// the caller (the ASCII skin passes its `mid` glyph so the HUD stays ASCII).
    pub fn thinking_label_with(&self, sep: &str) -> Option<String> {
        let level = self.thinking?;
        if !is_claude_55(&self.model) {
            return Some(level.as_str().to_string());
        }
        let eff = if level == ThinkingLevel::Off {
            "low"
        } else {
            "high"
        };
        Some(format!("adaptive {sep} {eff}"))
    }
}

/// True for the Claude 5.5 families (`claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5`),
/// optionally behind `anthropic/` and with a `-`-separated suffix such as a date. Other names
/// that merely contain `-5-5` (`claude-haiku-5-50`, `claude-5-5-x`) do not match.
pub(crate) fn is_claude_55(model: &str) -> bool {
    let m = model.strip_prefix("anthropic/").unwrap_or(model);
    ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]
        .iter()
        .any(|family| {
            m.strip_prefix(family)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
        })
}

#[cfg(test)]
mod thinking_tests {
    use super::*;

    #[test]
    fn claude_55_matcher_edges() {
        for yes in [
            "anthropic/claude-opus-5-5",
            "anthropic/claude-sonnet-5-5",
            "anthropic/claude-haiku-5-5",
            "claude-haiku-5-5",
            "anthropic/claude-haiku-5-5-20261001",
            "anthropic/claude-sonnet-5-5-v2",
        ] {
            assert!(is_claude_55(yes), "{yes} should match");
        }
        for no in [
            "anthropic/claude-haiku-5-50",
            "anthropic/claude-5-5-x",
            "anthropic/claude-haiku-5-5x",
            "anthropic/claude-haiku-4-5",
            "anthropic/claude-opus-5-55",
            "openrouter/claude-haiku-5-5",
            "qwen-local",
            "",
        ] {
            assert!(!is_claude_55(no), "{no} should not match");
        }
    }

    #[test]
    fn thinking_label_table() {
        let levels = [
            (ThinkingLevel::Off, "adaptive • low"),
            (ThinkingLevel::Minimal, "adaptive • high"),
            (ThinkingLevel::Low, "adaptive • high"),
            (ThinkingLevel::Medium, "adaptive • high"),
            (ThinkingLevel::High, "adaptive • high"),
            (ThinkingLevel::Xhigh, "adaptive • high"),
            (ThinkingLevel::Max, "adaptive • high"),
        ];
        for (level, want) in levels {
            let st = StatusModel {
                model: "anthropic/claude-haiku-5-5".to_string(),
                thinking: Some(level),
                ..StatusModel::default()
            };
            assert_eq!(st.thinking_label().as_deref(), Some(want), "5.5 level {level:?}");
            let plain = StatusModel {
                model: "qwen-local".to_string(),
                thinking: Some(level),
                ..StatusModel::default()
            };
            assert_eq!(plain.thinking_label().as_deref(), Some(level.as_str()));
        }
    }
}

/// #873: the unit a budget is counted in. The HUD spells it out (`$`, `tok`), so a number is
/// never read as the wrong kind of budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetUnit {
    /// US dollars, two decimals.
    Usd,
    /// Tokens, compact (`12k`, `1.5M`).
    Tokens,
}

/// #873: what a budget covers. The HUD spells it out (`day`, `run`, `goal`), so a used/cap
/// pair is never read as a different scope than the one it was projected from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetScope {
    /// The gateway's per-day cloud budget (`cloud.daily_budget_usd`).
    Day,
    /// One invocation's soft token budget (`--max-tokens`).
    Run,
    /// A stored goal's budget (`oxa.goal.v1`).
    Goal,
}

/// #873: one authoritative budget the HUD can show: what is used against what cap, in which
/// unit and for which scope. A front end builds it from an authoritative projection (the
/// gateway's daily USD budget, an invocation token budget, a goal budget); the HUD never
/// computes a budget of its own, never forecasts and never issues a request.
///
/// The projection is optional and may be partial: `used: None` means settlement is
/// unavailable, and the HUD shows `?` rather than a zero. `incomplete` means some usage is
/// not settled yet, so `used` is a lower bound and the HUD marks it (`8.10+`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetMeter {
    /// Settled usage so far, in `unit`; `None` = settlement is unavailable (never a zero).
    pub used: Option<f64>,
    /// The cap, or `None` when no cap is set (unlimited or not configured).
    pub cap: Option<f64>,
    pub unit: BudgetUnit,
    pub scope: BudgetScope,
    /// Some usage is not settled yet, so `used` is a lower bound. Ignored when `used` is
    /// `None` (an unknown settlement has no lower bound to mark).
    pub incomplete: bool,
}

/// Everything the chrome (D6) draws. Updated by [`crate::apply_event`] and by the app from
/// `Session::state()` / `Session::stats()` after each settle.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StatusModel {
    pub cwd: String,
    /// The home directory (`Dirs::home`), for showing paths as `~`; empty = no shortening.
    /// Display only. Views never read the process environment (P56).
    pub home: String,
    /// Git branch of cwd, display only (never on the wire).
    pub git_branch: Option<String>,
    pub session_name: Option<String>,
    pub model: String,
    /// Wave D D4b: where the model runs (`local` or `cloud · <provider>`), display only.
    pub model_remote: bool,
    pub model_provider: Option<&'static str>,
    pub thinking: Option<ThinkingLevel>,
    /// Why thinking is forced off for this model (W6), shown in the status line.
    pub thinking_note: Option<String>,
    pub mode: Option<Mode>,
    /// Exact context tokens after the last reply and the context window.
    pub context_tokens: u32,
    pub n_ctx: u32,
    /// `cached / prompt` of the last request (cache hit), when known.
    pub last_cached: Option<u32>,
    pub last_prompt: Option<u32>,
    /// Decode speed of the last reply in tokens per second.
    pub tok_per_s: Option<f64>,
    /// Cumulative tokens of the session: input, output.
    pub total_in: u64,
    pub total_out: u64,
    pub activity: Activity,
    /// Milliseconds since the current activity began (the app's clock).
    pub activity_ms: u64,
    pub steering: Vec<String>,
    pub follow_up: Vec<String>,
    /// Extension status entries (`Ui::set_status`), by key.
    pub ext_status: BTreeMap<String, String>,
    /// `Ui::set_working_message`.
    pub working_message: Option<String>,
    pub auto_compact: bool,
    /// Count of cache misses with an unknown cause in this process (should stay 0).
    pub unknown_misses: u32,
    /// T2: malformed-call repairs of the session (`Stats::tool_repairs`); shown when not 0.
    pub tool_repairs: u32,
    /// The last compaction had no model summary (`details.mechanical`); shown in the footer
    /// until a compaction with a model summary, or a new session.
    pub degraded_context: bool,
    /// O-182: YOLO is on (prompts are auto-approved) and how many were approved. Display only;
    /// set by the UI loop from the process's `YoloSwitch`.
    pub yolo: bool,
    pub yolo_approved: u64,
    /// #202: the process's autonomy level (standard, auto or yolo); with `mode` it gives the
    /// permission mode the status bar shows.
    pub autonomy: oxa_proto::Autonomy,
    /// #873: the authoritative budget the HUD shows, when a front end has one. `None` = no
    /// budget projection is available, and the HUD shows no budget part at all (never a zero).
    /// The projection itself is the public budget surface of #627; until it lands no front end
    /// sets this, so the part is absent in a real session.
    pub budget: Option<BudgetMeter>,
}

impl StatusModel {
    /// #202: the permission mode as the status bar shows it, with its theme token: `plan` and
    /// `ask` from the session mode, else the autonomy level (`standard`, `auto`, `yolo`).
    pub fn mode_label(&self) -> Option<(&'static str, &'static str)> {
        let (label, token) = self.mode_label_base()?;
        Some((mark_no_os_sandbox(label, oxa_proto::OS_SANDBOX), token))
    }

    fn mode_label_base(&self) -> Option<(&'static str, &'static str)> {
        use oxa_proto::Autonomy;
        Some(match self.mode? {
            Mode::Plan => ("plan", "modePlan"),
            Mode::Ask => ("ask", "modeAsk"),
            Mode::Workspace => match self.autonomy {
                Autonomy::Standard => ("standard", "modeWorkspace"),
                Autonomy::Auto => ("auto", "modeWorkspace"),
                Autonomy::Yolo => ("yolo", "error"),
            },
        })
    }
}

/// The status-bar mode label, with the persistent "no OS sandbox" marker when the platform has
/// no sandbox for bash. With a sandbox the label is returned unchanged.
pub fn mark_no_os_sandbox(label: &'static str, os_sandbox: bool) -> &'static str {
    if os_sandbox {
        return label;
    }
    match label {
        "plan" => "plan, no OS sandbox",
        "ask" => "ask, no OS sandbox",
        "standard" => "standard, no OS sandbox",
        "auto" => "auto, no OS sandbox",
        "yolo" => "yolo, no OS sandbox",
        other => other,
    }
}

/// The look of the interactive UI. `Oxa` is OXA's own look (frames, HUD line, hint bar;
/// OXA-UI-DESIGN.md); `OxaAscii` is the same with ASCII glyphs for terminals without UTF-8.
/// `Pi` is the plain look (the parity reference), reachable with `ui.skin = "pi"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Skin {
    #[default]
    Pi,
    Oxa,
    OxaAscii,
}

impl Skin {
    pub fn is_oxa(self) -> bool {
        !matches!(self, Skin::Pi)
    }
}

/// What the views need to know from settings (resolved once per settings change by the app).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewSettings {
    /// `hideThinkingBlock`.
    pub hide_thinking: bool,
    /// Collapsed tool output shows at most this many lines.
    pub collapsed_tool_lines: usize,
    /// All tool blocks expanded (the `app.tools.expand` toggle).
    pub tools_expanded: bool,
    /// `images.blockImages` or no image protocol: draw the fallback text.
    pub block_images: bool,
    /// `markdown.codeBlockIndent`.
    pub code_block_indent: u16,
    /// `markdown.codeBlockIndent` as pi's prefix string (when set, instead of the spaces).
    pub code_block_prefix: Option<String>,
    /// `outputPad` (0 or 1): columns of padding left and right of transcript blocks.
    pub output_pad: u16,
    /// `terminal.imageWidthCells`: the widest an inline image is drawn.
    pub image_width_cells: u16,
    /// `quietStartup`.
    pub quiet_startup: bool,
    /// Scrollbar mode in fullscreen: `"auto"`, `"always"`, `"never"`.
    pub scrollbar: String,
    /// Lines per mouse wheel notch.
    pub wheel_lines: u16,
    /// Copy the selection to the clipboard when the mouse button is released.
    pub copy_on_select: bool,
    pub hyperlinks: bool,
    /// The look (`ui.skin` / `OXA_SKIN`, else OXA's).
    pub skin: Skin,
}

impl Default for ViewSettings {
    fn default() -> Self {
        ViewSettings {
            hide_thinking: false,
            collapsed_tool_lines: 10,
            tools_expanded: false,
            block_images: true,
            code_block_indent: 2,
            code_block_prefix: None,
            output_pad: 1,
            image_width_cells: 60,
            quiet_startup: false,
            scrollbar: "auto".to_string(),
            wheel_lines: 3,
            copy_on_select: false,
            hyperlinks: false,
            skin: Skin::Pi,
        }
    }
}

/// The whole display state. Pure data; `apply_event` is its only writer besides the app loop.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ViewModel {
    pub blocks: Vec<Block>,
    pub status: StatusModel,
    /// Extension widgets (`Ui::set_widget`): key -> lines, above and below the editor.
    pub widgets_above: BTreeMap<String, Vec<String>>,
    pub widgets_below: BTreeMap<String, Vec<String>>,
    /// `Ui::set_header` / `Ui::set_footer` replacements.
    pub header_override: Option<Vec<String>>,
    pub footer_override: Option<Vec<String>>,
    /// Window title set by an extension.
    pub title_override: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Commands to the worker thread
// ---------------------------------------------------------------------------------------------

/// How to open the next session (`/new`, `/resume`, `/fork`, `/clone`).
pub enum Reopen {
    New,
    Resume(SessionRef),
    /// Fork the given session file into a new one and open it.
    Fork(SessionRef),
}

/// Builds the next session for [`Reopen`]. The second argument carries the model and thinking
/// level of the session being left (`Some` only for [`Reopen::New`], which keeps them; the
/// other modes restore the target session's own, so the caller passes `None`).
pub type ReopenFn =
    Box<dyn FnMut(Reopen, Option<(String, ThinkingLevel)>) -> Result<Session, OxaError> + Send>;

/// One request to the worker thread that owns the `Session`. Each maps to one
/// `oxa_core::Session` call; the results come back as `AgentEvent`s and [`WorkerMsg`]s.
pub enum SessionCmd {
    /// Coalesced wake for editor input. The handle binds it to the original ordinary queues;
    /// a wake left behind by a session rebind must not run the replacement's input.
    QueuedInput(oxa_core::Handle),
    /// Explicit input after a cancel key belongs to the canceled session, not a queued replacement.
    RecoveryPrompt {
        handle: oxa_core::Handle,
        text: String,
        parts: Vec<Part>,
    },
    /// Event-driven inbox wake. The id makes a wake queued before a swap harmless.
    Inbox {
        session_id: String,
    },
    /// A finished background job's wake (W4). Unlike `Inbox` it honours the stop boundary: a
    /// run that ended failed, blocked or interrupted (`oxa.maxTurns` among them) keeps the
    /// message queued instead of starting a fresh run with a fresh turn budget.
    JobWake {
        session_id: String,
    },
    /// The worker's own wake, sent by the session's attention waker
    /// (`Handle::set_attention_waker`): a host request was posted while the worker waited, or
    /// a wrap-up was requested. No command runs; the worker serves the host now instead of at
    /// its next idle tick.
    Wake,
    /// `Session::prompt`. Slash commands that are templates, `/skill:name` or extension
    /// commands are sent as a prompt and expanded by the core.
    Prompt {
        text: String,
        parts: Vec<Part>,
    },
    Compact {
        instructions: Option<String>,
    },
    /// `/goal [text|clear|pause|resume]`: the text after the command, unparsed (G1 parses it).
    Goal(String),
    /// `Session::navigate`; `instructions` = the custom focus of the branch summary (D-INT).
    Navigate {
        target: String,
        summarize: bool,
        instructions: Option<String>,
    },
    /// Ask for the session tree; answered with [`WorkerMsg::Tree`] (D-INT: `/tree`, `/fork`).
    Snapshot,
    /// Fork before a user-message entry (pi's default position) into a new session file and
    /// switch to it.
    ForkAt {
        entry: String,
    },
    CloneSession,
    SetModel(String),
    SetThinking(ThinkingLevel),
    SetMode(Mode),
    SetName(String),
    /// `/export [path]` as HTML (pi): the worker writes the session to `path`, an absolute
    /// path, and reports the outcome as a notice (`app::export_html_file`).
    Export {
        path: PathBuf,
    },
    SetLabel {
        target: String,
        label: Option<String>,
    },
    Reload,
    UserBash {
        command: String,
        excluded: bool,
    },
    /// Close the current session and open another through `InteractiveOpts::reopen`.
    Reopen(Reopen),
    /// A key bound by an extension (`registerShortcut`) that no built-in binding consumed:
    /// `Session::run_shortcut` (E-INT open item 3).
    Shortcut(oxa_proto::KeyId),
    /// Fire `session_shutdown` and end the worker.
    Shutdown,
    /// V020-OPTIMIZE #13: prefill the editor's draft (`Session::prefill_draft`) while the user
    /// still composes. `gen` is the draft generation it was taken at
    /// (`WorkerStop::draft_gen`): the worker skips it, or cancels it, once the draft changed.
    /// Answered with nothing (no `Done`).
    PrefillDraft {
        text: String,
        parts: Vec<Part>,
        gen: u64,
    },
}

/// What the worker sends back besides `AgentEvent`s.
pub enum WorkerMsg {
    Event(oxa_proto::AgentEvent),
    /// A command finished; `Err` is shown as an error notice.
    Done(Result<(), OxaError>),
    /// The watched session reached its terminal wrap-up boundary.
    LimitExit(i32),
    /// A session switch overtook explicit recovery input; return it without sending it.
    UnsentPrompt {
        text: String,
        parts: Vec<Part>,
    },
    /// The session was swapped; the app rebuilds the view model from the new branch.
    Reopened {
        entries: Vec<oxa_proto::Entry>,
        status: StatusModel,
    },
    /// Fresh status after a settle.
    Status(StatusModel),
    /// Which session the worker holds now (D-INT): sent once when the worker starts and after
    /// every swap (`/new`, `/resume`, `/fork`, `/clone`) and reload.
    Session(SessionInfo),
    /// The answer to [`SessionCmd::Snapshot`].
    Tree(oxa_session::Tree),
    /// The worker opened another `Session` object (`/new`, `/resume`): this is its
    /// control handle (D-INT). Abort, steering and follow-ups go to it from now on; what was
    /// still queued for the previous session is handed back to the user, never sent.
    Rebind(oxa_core::Handle),
    /// The worker asks the UI to open a selector (S3: "Fork from an earlier message" in the
    /// over-cap dialog of a model switch opens the fork selector).
    Open(SelectorKind),
}

/// The identity of the worker's session and what the UI thread needs from it without owning
/// the `Session` (D-INT).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionInfo {
    /// Path of the session file (`None` = ephemeral).
    pub file: Option<PathBuf>,
    pub id: String,
    /// `Session::commands()`: built-ins, extension commands, templates and skills, in pi's
    /// clash order (autocomplete, `SlashCx::commands`).
    pub commands: Vec<oxa_proto::CommandSpec>,
    /// The engine endpoint and the models it can serve (`EngineInfo`).
    pub engine_url: String,
    /// The observed memory policy of an in-process local engine child (`scope`, or
    /// `inherited` under a no-swap service). `None` for an engine that does not observe one.
    /// Separate from `engine_url`, which stays the endpoint's own name for comparison.
    pub memory_policy: Option<&'static str>,
    pub models: Vec<String>,
    /// Requests waiting on the engine, as of the last `Engine::info`.
    pub queue: u32,
    /// Keys bound by hook sets (`Hooks::shortcuts`), sent to the worker as
    /// [`SessionCmd::Shortcut`] when no built-in binding has them.
    pub shortcuts: Vec<oxa_proto::KeyId>,
}

// ---------------------------------------------------------------------------------------------
// UI requests (worker thread -> UI thread)
// ---------------------------------------------------------------------------------------------

/// One `oxa_proto::Ui` call, relayed from the worker thread. Variants with a `reply` block
/// the worker until answered; dropping the sender without answering counts as `None` / `Deny`.
pub enum UiRequest {
    Select {
        title: String,
        options: Vec<String>,
        timeout_ms: Option<u32>,
        reply: Sender<Option<usize>>,
    },
    Confirm {
        title: String,
        body: String,
        timeout_ms: Option<u32>,
        reply: Sender<Option<bool>>,
    },
    /// The publication approval dialog (`Ui::approve_publication`): exact text, Approve/Deny,
    /// Deny focused, no timeout.
    ApprovePublication {
        title: String,
        body: String,
        reply: Sender<Option<bool>>,
    },
    Input {
        title: String,
        placeholder: String,
        reply: Sender<Option<String>>,
    },
    Editor {
        title: String,
        prefill: String,
        reply: Sender<Option<String>>,
    },
    AskAccess {
        access: Access,
        reply: Sender<Answer>,
    },
    /// #1512 S2: the dangerous-command prompt. `keys` are the grant keys of `reasons`; `Always`
    /// saves them to the user settings (see `driver::append_always`).
    AskDangerous {
        access: Access,
        reasons: Vec<String>,
        keys: Vec<String>,
        reply: Sender<DangerAnswer>,
    },
    Custom {
        component: Box<dyn UiComponent>,
        opts: CustomOpts,
        reply: Sender<Option<Value>>,
    },
    EditorText {
        reply: Sender<String>,
    },
    Notify {
        text: String,
        level: Level,
    },
    SetStatus {
        key: String,
        text: Option<String>,
    },
    SetWidget {
        key: String,
        lines: Option<Vec<String>>,
        at: Placement,
    },
    SetTitle(String),
    SetEditorText(String),
    SetFooter(Option<Vec<String>>),
    SetHeader(Option<Vec<String>>),
    SetWorkingMessage(Option<String>),
}

// ---------------------------------------------------------------------------------------------
// App actions (what a key, a slash command or a selector asks the app to do)
// ---------------------------------------------------------------------------------------------

/// The selectors and overlays the app can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectorKind {
    Model,
    ScopedModels,
    Thinking,
    Theme,
    Settings,
    Sessions,
    Tree,
    /// Pick a user message to fork from (`/fork`).
    ForkPoint,
    Mcp,
    Trust,
    Hotkeys,
    /// #202: the permission-mode picker (`/mode` without an argument).
    Mode,
}

/// Everything that can happen as the result of user input. Produced by the slash dispatcher
/// (D11), the selectors (D7-D9, F7) and key handling (D1); carried out by the app loop (D1).
pub enum AppAction {
    /// Nothing to do.
    None,
    Session(SessionCmd),
    Open(SelectorKind),
    /// O-182: switch YOLO on or off (`None` = toggle).
    Yolo(Option<bool>),
    /// #202: set the permission mode (`/mode <name>`, `/auto`, the picker); `save` also writes
    /// it as `permissions.defaultMode`.
    SetPermissionMode {
        mode: oxa_proto::PermissionMode,
        save: bool,
    },
    /// Show a notice block.
    Notify {
        level: Level,
        text: String,
    },
    /// Show plain lines as an info block (`/hotkeys`, `/session`, `/engine`).
    ShowLines(Vec<String>),
    SetEditorText(String),
    /// Copy text to the clipboard (`/copy`).
    Copy(String),
    /// `/export [path]`.
    Export {
        path: Option<PathBuf>,
    },
    /// Write the `/debug` file and report its path.
    DebugDump,
    /// `/bug <description>`: write the local, redacted bug report bundle and report its path
    /// (D-INT). Nothing is uploaded.
    BugReport {
        description: String,
    },
    /// Change a setting and persist it (`scope_project` = project settings file).
    SaveSetting {
        key: String,
        value: Value,
        scope_project: bool,
    },
    /// Switch the theme now (preview or final).
    SetTheme {
        name: String,
        persist: bool,
    },
    McpRequest(crate::McpRequest),
    /// Rename or delete a session file from the picker.
    RenameSession {
        session: SessionRef,
        name: String,
    },
    DeleteSession {
        session: SessionRef,
    },
    /// Answer the trust question for the current project.
    SetTrust(bool),
    /// Toggle or clear display state.
    ToggleToolsExpanded,
    ToggleThinkingHidden,
    /// Open the external editor on the prompt text.
    ExternalEditor,
    Suspend,
    Abort,
    /// Abort a pending retry wait only.
    AbortRetry,
    /// Bring queued messages back into the editor (`app.queue.dequeue`, Alt+Up).
    Dequeue,
    Quit,
    /// Several actions, in order.
    Many(Vec<AppAction>),
}

// ---------------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------------

/// Saves a `/trust` decision (see `save_trust`).
pub type TrustSaver = Box<dyn FnMut(crate::TrustChoice) -> Result<(), OxaError> + Send>;

/// Optional owner-only details read on demand by the existing engine command.
pub type EngineStatus = Box<dyn Fn() -> Vec<String> + Send>;

/// Everything `run` needs besides the session. Built by `oxa-cli`.
pub struct InteractiveOpts {
    /// `true` = alternate screen layout, `false` = regular (main screen).
    pub fullscreen: bool,
    pub dirs: Dirs,
    pub settings: Settings,
    pub keymap: Keymap,
    pub store: Arc<dyn SessionStore>,
    /// The hook sets of the session (renderers, shortcuts, completions).
    pub hooks: Vec<Arc<dyn Hooks>>,
    pub mcp: Option<Arc<McpManager>>,
    /// A prompt to send right after start (CLI message arguments, piped stdin).
    pub initial_prompt: Option<String>,
    pub initial_parts: Vec<Part>,
    /// Further messages from the command line, sent as follow-ups in order.
    pub initial_follow_ups: Vec<String>,
    /// Initial theme override (a name or a path).
    pub theme: Option<String>,
    /// Ordered discovered and explicit theme files/directories (later paths win).
    pub theme_paths: Vec<std::path::PathBuf>,
    /// Startup notices from loading settings, resources and extensions.
    pub notices: Vec<(Level, String)>,
    /// Environment snapshot for capability detection, editor and clipboard choice.
    pub env: Vec<(String, String)>,
    /// Capability overrides resolved by the CLI; `None` = detect from `env`.
    pub caps: Option<TermCaps>,
    /// Builds the next session for `/new`, `/resume`, fork and clone. Runs on the
    /// worker thread after the previous session was dropped. The second argument is the model
    /// and thinking level of the session being left (only for [`Reopen::New`], which carries
    /// them over; `None` for the other modes, which restore the target session's own).
    pub reopen: ReopenFn,
    /// `-r` / `--resume`: open the session picker right after start (D-INT).
    pub open_picker: bool,
    /// Saves a `/trust` decision for the session's directory (D-INT; the CLI owns the trust
    /// store). `None` = trust cannot be changed here (tests, ephemeral setups).
    pub save_trust: Option<TrustSaver>,
    /// The gated project resources (`oxa_perm::gated_resources`), listed by `/trust` (D-INT).
    pub gated: Vec<String>,
    pub engine_status: Option<EngineStatus>,
    /// O-182: the process's YOLO switch, shared with the policy of every session. `None` =
    /// YOLO cannot be switched here (the policy does not listen; tests, ephemeral setups).
    pub yolo: Option<oxa_proto::YoloSwitch>,
}

#[cfg(test)]
mod no_os_sandbox_tests {
    use super::mark_no_os_sandbox;

    #[test]
    fn status_mode_label_marks_a_missing_sandbox_and_is_unchanged_with_one() {
        for l in ["plan", "ask", "standard", "auto", "yolo"] {
            assert_eq!(mark_no_os_sandbox(l, true), l);
            assert!(mark_no_os_sandbox(l, false).ends_with("no OS sandbox"));
        }
    }
}
