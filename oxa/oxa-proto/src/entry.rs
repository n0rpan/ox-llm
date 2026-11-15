use crate::{FrozenPrefix, Part, ThinkingLevel, Usage, Value, WireMsg};

/// Permission mode (plan sec. 2). Trust gates what loads; the mode gates what tools do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Read class + read-only bash.
    Plan,
    /// Default: writes inside cwd + sandboxed bash without prompts; the rest asks.
    Workspace,
    /// Each Write/Exec asks once per scope.
    Ask,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Plan, Mode::Workspace, Mode::Ask];
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Plan => "plan",
            Mode::Workspace => "workspace",
            Mode::Ask => "ask",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
    /// The next mode of the `app.mode.cycle` key: plan -> workspace -> ask -> plan.
    pub fn next(self) -> Mode {
        match self {
            Mode::Plan => Mode::Workspace,
            Mode::Workspace => Mode::Ask,
            Mode::Ask => Mode::Plan,
        }
    }
}

/// One line of a session file: the pi v3 envelope (`id` / `parentId` / `timestamp`) + a body.
///
/// The session is a tree; the wire is a pure fold of the active branch (W1). An entry may only
/// append to the wire, except the rebasing bodies documented on [`EntryBody`].
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// 8 lowercase hex characters, unique within the file (pi's scheme).
    pub id: String,
    /// `None` only for the first entry of a file.
    pub parent: Option<String>,
    /// Unix milliseconds. Written to the file as pi's ISO-8601 `timestamp`.
    pub ts_ms: u64,
    pub body: EntryBody,
}

/// Display-only data of a tool result that the wire message does not carry.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolMeta {
    pub is_error: bool,
    /// `ToolOutput::ui` (diff, exit code, duration); `Null` when the tool gave none.
    pub ui: Value,
}

/// The body of a session entry. The variant-to-`type` mapping on disk is fixed by
/// `oxa_session::encode_entry` (pi's entry type names; see `oxa-session/README.md`).
///
/// **Fold rule (W1).** `Transcript::project` appends for `Msg`, for the `BashExecution` /
/// `CustomMessage` bodies that are in context and for `BranchSummary` (one user message at its
/// position; the leaf move itself is the rebase); it rebases for `Prefix` (a later epoch),
/// `Compaction` and `ContextEdit`; every other body leaves the wire untouched.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryBody {
    /// pi type `message` with `role: "system"` and `sections` (pi stores its prompt this way);
    /// oxa adds `toolsJson` and `sha256`. The frozen prefix of an epoch (W3). The first entry of
    /// every session.
    Prefix(FrozenPrefix),
    /// pi type `message` (roles `user`, `assistant`, `toolResult`). `wire` is exactly what goes on the wire (W2: for an assistant entry
    /// these are the bytes as streamed); `parts` is what the user attached (shown, not sent).
    ///
    /// `tool` is set on `toolResult` entries only: the display half of the result (error flag,
    /// diff / exit code), so a resumed session renders it as it did live. Never sent.
    Msg {
        wire: WireMsg,
        parts: Vec<Part>,
        tool: Option<ToolMeta>,
    },
    /// oxa type `shown`. Text shown to the user and never sent: an aborted / failed / cut reply
    /// (W2) or a notice. `why` is one of [`crate::shown_why`].
    Shown { text: String, why: &'static str },
    /// pi type `message` with `role: "bashExecution"`: a `!cmd` (in context at the next prompt, as an append) or a
    /// `!!cmd` (`excluded`, never in context).
    BashExecution {
        command: String,
        output: String,
        exit: Option<i32>,
        excluded: bool,
    },
    /// pi type `model_change`. A model switch also starts a new epoch.
    ModelChange(String),
    /// pi type `thinking_level_change`.
    ThinkingLevelChange(ThinkingLevel),
    /// oxa type `mode_change`. Never on the wire (W3: no mode text).
    ModeChange(Mode),
    /// pi type `usage`: exact counts of one request, `kind` = a `TurnKind::as_str`.
    Usage { kind: String, usage: Usage },
    /// pi type `compaction`. Rebases the wire to `[user:<summary>, tail from first_kept]` (W5).
    Compaction {
        summary: String,
        first_kept: String,
        tokens_before: u32,
        details: Value,
        from_hook: bool,
    },
    /// pi type `context_edit`. Applying one is a rebase with a notice.
    ContextEdit {
        target: String,
        replacement: Option<String>,
    },
    /// pi type `branch_summary`: written at the new leaf when `/tree` leaves a branch.
    BranchSummary {
        from: String,
        summary: String,
        details: Value,
        from_hook: bool,
    },
    /// pi type `custom`: extension state, never in context.
    Custom { custom_type: String, data: Value },
    /// pi type `custom_message`: an extension message that is in context (an append, user role).
    CustomMessage {
        custom_type: String,
        content: String,
        display: bool,
        details: Value,
    },
    /// pi type `label`. `label: None` clears it.
    Label { target: String, label: Option<String> },
    /// pi type `session_info`: the display name.
    SessionInfo { name: String },
}
