use crate::{Access, CancelToken, Mode, Sandbox, Ui, Value, WireImage};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Permission class of a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolClass {
    Read,
    Write,
    Exec,
    Net,
}

/// How a tool is offered to the model (pi's MCP exposure modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Exposure {
    /// Declared in the frozen prefix's tool list.
    Direct,
    /// Callable from codemode scripts only (the MCP default). Never touches the prefix.
    Codemode,
    /// Found through `tool_search`, called through `call_tool`. Never touches the prefix.
    Deferred,
    Hidden,
}

impl Exposure {
    pub fn as_str(self) -> &'static str {
        match self {
            Exposure::Direct => "direct",
            Exposure::Codemode => "codemode",
            Exposure::Deferred => "deferred",
            Exposure::Hidden => "hidden",
        }
    }
    pub fn parse(s: &str) -> Option<Exposure> {
        Some(match s {
            "direct" => Exposure::Direct,
            "codemode" => Exposure::Codemode,
            "deferred" => Exposure::Deferred,
            "hidden" => Exposure::Hidden,
            _ => return None,
        })
    }
}

/// Where a tool or command comes from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Source {
    Builtin,
    /// Extension path or `builtin:<name>`.
    Extension(String),
    /// MCP server name.
    Mcp(String),
}

/// A tool as declared to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// The JSON Schema of the arguments, as canonical JSON text (fixed key order; part of the
    /// frozen prefix bytes).
    pub schema_json: String,
    pub class: ToolClass,
    pub exposure: Exposure,
    pub source: Source,
}

/// What a tool returns.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// The text for the model. The loop passes it through `oxa_tools::shape` before the push
    /// (W4); a tool must still be deterministic: sorted, no timings, no ANSI.
    pub text: String,
    pub is_error: bool,
    /// For display only (diff, exit code, duration). **Never reaches the wire.**
    pub ui: Value,
    /// Images the tool returns (B2a: the type; `read` fills it in B2b). They belong to the tool
    /// result message, after `text`, and are the only tool output that may reach the wire as
    /// an image part. Empty for every other tool.
    pub images: Vec<WireImage>,
}

impl ToolOutput {
    pub fn ok(text: impl Into<String>) -> Self {
        ToolOutput {
            images: Vec::new(),
            text: text.into(),
            is_error: false,
            ui: Value::Null,
        }
    }
    pub fn error(text: impl Into<String>) -> Self {
        ToolOutput {
            images: Vec::new(),
            text: text.into(),
            is_error: true,
            ui: Value::Null,
        }
    }
}

/// Fixed tool-error texts produced by the loop itself (deterministic, part of W4).
pub mod tool_errors {
    /// `{name}` is replaced by the requested tool name.
    pub const UNKNOWN_TOOL: &str = "Tool {name} not found";
    /// The argument string is not a JSON object.
    pub const BAD_JSON: &str = "Invalid tool arguments: expected a JSON object";
    /// Prefix of a policy denial; followed by the `Decision::Deny` reason.
    pub const DENIED_PREFIX: &str = "Permission denied: ";
    pub const DENIED_BY_USER: &str = "Permission denied: the user declined this action";
    /// #554: an action that needs approval in a run with no one to ask (print/json). `{what}` is
    /// the action ("write /tmp/x"). Nobody declined; avoid workarounds whose approval depends
    /// on the active policy and path protections.
    pub const DENIED_UNATTENDED: &str = "Permission denied: {what} needs approval, but this run cannot ask (non-interactive), so it was refused; nobody declined it.";
    /// #1512 S2: a dangerous command in a run with no one to ask. `{why}` is the matched rules.
    pub const DANGEROUS_UNATTENDED: &str =
        "Refused: dangerous command ({why}). This run has no one to approve it; nobody declined it.";
    /// #1512 S5: an unattended run under the OS sandbox refuses a dangerous command at once.
    /// `{why}` is the matched rules.
    pub const DANGEROUS_REFUSED: &str = "Refused: dangerous command ({why}). Unattended runs with the OS sandbox refuse dangerous commands. Do the task another way, or report that the owner must run it.";
    /// #1512 S5: the owner setting `oxa.approval.unattended = "refuse"` refuses a dangerous
    /// command in an unattended run without the OS sandbox. `{why}` is the matched rules.
    pub const DANGEROUS_REFUSED_SETTING: &str = "Refused: dangerous command ({why}). The owner setting oxa.approval.unattended refuses dangerous commands in unattended runs. Do the task another way, or report that the owner must run it.";
    /// #1512 S5: the approval request could not be filed (the approval desk failed), so the
    /// command is refused. `{why}` is the matched rules.
    pub const DANGEROUS_REFUSED_NO_REQUEST: &str = "Refused: dangerous command ({why}). An approval request could not be filed for this unattended run. Do the task another way, or report that the owner must run it.";
    /// #1512 S5: an unattended run without the OS sandbox files an approval request instead.
    /// `{id}` is the request id, `{session}` the session, `{why}` the matched rules.
    pub const DANGEROUS_REQUESTED: &str = "Needs owner approval (request {id}): {why}. The owner answers with `oxa send {session} --approve {id} once|session|always|deny` or on the lead web page. Continue with other work and retry this exact command after it is approved.";
    pub const ABORTED: &str = "Tool execution aborted";
    /// A call in a reply cut by the output limit (`Finish::Length`) is never run (pi's
    /// `failToolCallsFromTruncatedMessage`). `{name}` is replaced by the tool name.
    pub const TRUNCATED: &str = "Tool call \"{name}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.";
    /// A1/A2: the tool panicked (a bug in the tool, not in the call); the turn goes on with
    /// this result. `{name}` is the tool name, `{error}` the panic message.
    pub const PANICKED: &str = "Tool {name} failed with an internal error: {error}";
}

/// What a tool may use while it runs.
pub struct ToolCtx<'a> {
    /// Absolute working directory of the session.
    pub cwd: &'a Path,
    /// Where `shape` puts spill files.
    pub spill_dir: &'a Path,
    pub sandbox: &'a dyn Sandbox,
    pub mode: Mode,
    pub cancel: &'a CancelToken,
    /// Environment for child processes (`OXA_SESSION_*`, `AI_AGENT=oxa`).
    pub env: &'a [(String, String)],
    /// How the built-ins shape their results in this epoch ([`ToolTune::default`] = the
    /// unshaped baseline; `diet` and the page caps shape it further).
    pub tune: ToolTune,
    /// #518: for each path the call's `Tool::access` named and the permission check allowed,
    /// the real location the check judged (symlinks resolved, [`Policy::decide_pinned`]). A file
    /// tool opens only that location: a link swapped after the check is refused, not followed.
    /// Empty when nothing was checked (a direct call).
    ///
    /// [`Policy::decide_pinned`]: crate::Policy::decide_pinned
    pub checked: &'a [(PathBuf, PathBuf)],
    /// #517: the paths no tool may show (the policy's deny-list, [`Policy::path_filter`]).
    /// The policy only judges the path a tool names, so a tool that walks a directory
    /// (`grep`, `find`, `ls`) skips what this hides. `None` = nothing hidden.
    ///
    /// [`Policy::path_filter`]: crate::Policy::path_filter
    pub hidden: Option<Arc<dyn PathFilter>>,
}

impl ToolCtx<'_> {
    /// #518: the real location the permission check judged for `path` (as `Tool::access`
    /// reported it), if it was checked.
    pub fn checked_real(&self, path: &Path) -> Option<&Path> {
        self.checked
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, real)| real.as_path())
    }
}

/// #517: the deny-list as a cheap check for the entries of a directory walk.
pub trait PathFilter: Send + Sync + std::fmt::Debug {
    /// `real` (absolute, its directories already resolved: the walk resolves its root once and
    /// does not follow links) is on the deny-list and must not be listed or opened.
    fn hides(&self, real: &Path) -> bool;
}

/// Per-epoch tuning of the built-in tools (V020-OPTIMIZE #2, OXA-LOCAL-FIRST T7, T8, F5, R5).
/// The core computes it once per prefix epoch from the settings and the model's
/// limits; a tool only obeys it. `Default` keeps pi 1.5.2's numbers but not its texts: the
/// built-ins always use the tolerant `edit` match and the read-class shaping below, so W4's
/// same-bytes rule holds per value (same arguments + same file state + same `ToolTune` = the
/// same bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolTune {
    /// TOOL-DIET (`oxa.diet.tools`): lossless shaping of read-class results (path once per
    /// file in `grep`, runs of blank lines collapsed in `bash` output, short `edit` / `write`
    /// acknowledgements).
    pub diet: bool,
    /// `read` without `offset` / `limit` on a file larger than one page: lines of the first
    /// page (T8, R5). 0 = pi (2000 lines).
    pub read_page_lines: u32,
    /// As `read_page_lines`, in bytes. 0 = the W4 cap.
    pub read_page_bytes: u32,
    /// `bash`: timeout in seconds when the call sets none (F5). 0 = none (pi).
    pub bash_timeout_s: u32,
    /// L4: the tool output cap in bytes. `grep`, `find`, `ls` and a `read` with
    /// `offset` / `limit` cut their result at it (never above pi's 16 KiB), notices inside it.
    /// 0 = none (pi: each tool's own 16 KiB cut).
    pub output_cap_bytes: u32,
}

/// A tool the model can call. Built-ins, extension tools, MCP tools and codemode all implement
/// this one trait.
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    /// What running with `args` would touch. Pure and cheap: called before every run so the
    /// policy can decide. `Err` = the arguments are invalid; the text becomes the tool error.
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String>;
    /// Run. Never panics on bad input: invalid arguments, missing files and failed commands are
    /// `ToolOutput { is_error: true, .. }`. `live` receives partial output for display. Must
    /// honour `cx.cancel`.
    fn run(&self, args: &Value, cx: &ToolCtx, live: &mut dyn FnMut(&str)) -> ToolOutput;
    /// `true` for a tool that opens dialogs while it runs (an extension tool that asks the
    /// user). The loop then calls [`Tool::run_with_ui`] instead of `run`, serially, never in a
    /// parallel batch.
    fn needs_ui(&self) -> bool {
        false
    }
    /// `run` with access to the user. Only called when [`Tool::needs_ui`] is true.
    fn run_with_ui(
        &self,
        args: &Value,
        cx: &ToolCtx,
        live: &mut dyn FnMut(&str),
        _ui: &mut dyn Ui,
    ) -> ToolOutput {
        self.run(args, cx, live)
    }
    /// `true` for a tool from a hook set that belongs to oxa's DEFAULT tool selection, like the
    /// built-ins: selected when no selection is given, kept or dropped by `+name` / `-name`
    /// edits, left out by an allowlist that does not name it and by `--no-builtin-tools` /
    /// `--no-tools` (see `oxa_core::resolve_toolset`). The hook set decides whether it offers
    /// the tool at all (the `subagent` tool: O-146). Default `false`: an ordinary extension tool.
    fn default_selected(&self) -> bool {
        false
    }
}

/// A slash command as listed by `get_commands` and the autocomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub source: Source,
}
