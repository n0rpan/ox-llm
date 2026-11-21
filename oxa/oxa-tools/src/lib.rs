//! W0-FROZEN (architect-owned).
//!
//! # oxa-tools
//! The seven built-in tools (`read`, `bash`, `edit`, `write`, `grep`, `find`, `ls`) and
//! [`shape`], the deterministic output cap.
//!
//! ## Contracts
//! * **W4.** Same arguments + same file state = the same bytes, on every run and machine:
//!   results are sorted, carry no timings, no absolute temp paths other than the spill line,
//!   no ANSI. Everything the model sees from any tool (built-in, MCP, extension, codemode)
//!   passes through [`shape`] in the loop; tools do their own pi-style truncation first
//!   (limits in [`descriptions::limits`]).
//! * Names, schemas, descriptions and error texts are pi's ([`descriptions`], architect-owned).
//!   A tool never invents an error text that pi has a fixed one for.
//! * grep and find run in-process (`ignore` + `regex` + `globset`); nothing is downloaded.
//! * A tool never panics on bad input and never reads the process environment.
//! * Paths: relative paths resolve against `ToolCtx::cwd`; `~` expands to nothing special
//!   (the sandbox hides home). `Tool::access` reports absolute, lexically normalised paths.

mod ask;
mod bash;
mod checked;
mod ci_poll;
mod deliverable;
pub mod descriptions;
mod diff;
mod edit;
mod find;
mod grep;
mod jobs;
mod ls;
mod read;
mod shape;
mod write;

pub use ask::AskTool;
pub use bash::{net_for as bash_net_for, BashTool, ShellConfig};
pub use deliverable::{observe as observe_deliverable, DeliverableTool, State as DeliverableState};
pub use diff::{apply_edits, detect_line_ending, strip_bom, unified_diff, Edit, EditOutcome};
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use jobs::{JobDone, JobInfo, JobRead, JobRegistry, JobStatus, TakenJob, BUFFER_CAP, MAX_JOBS, READ_CAP};
pub use ls::LsTool;
pub use read::ReadTool;
pub use shape::{shape, strip_ansi};
pub use write::WriteTool;

use descriptions::ToolText;
use oxa_proto::{Exposure, Source, Tool, ToolSpec};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// The [`ToolSpec`] of a built-in from its frozen text.
pub fn spec_of(t: &ToolText) -> ToolSpec {
    ToolSpec {
        name: t.name.to_string(),
        description: t.description.to_string(),
        schema_json: t.schema_json.to_string(),
        class: t.class,
        exposure: Exposure::Direct,
        source: Source::Builtin,
    }
}

/// All seven built-ins, in pi's order: read, bash, edit, write, grep, find, ls.
pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadTool::new()),
        Arc::new(BashTool::new()),
        Arc::new(EditTool::new()),
        Arc::new(WriteTool::new()),
        Arc::new(GrepTool::new()),
        Arc::new(FindTool::new()),
        Arc::new(LsTool::new()),
    ]
}

/// H: [`builtin_tools`] with the `bash` tool configured by `shell` (pi's `shellPath` and
/// `shellCommandPrefix`).
pub fn builtin_tools_with_shell(shell: ShellConfig) -> Vec<Arc<dyn Tool>> {
    let mut tools = builtin_tools();
    tools[1] = Arc::new(BashTool::with_shell(shell));
    tools
}

/// [`builtin_tools_with_shell`] with the `bash` tool sharing `jobs` (issue #127 (b)): the
/// background jobs the tool starts belong to the session that owns `jobs`, which kills
/// them all when it ends.
pub fn builtin_tools_with_shell_and_jobs(shell: ShellConfig, jobs: Arc<JobRegistry>) -> Vec<Arc<dyn Tool>> {
    let mut tools = builtin_tools();
    tools[1] = Arc::new(BashTool::with_shell_and_jobs(shell, jobs));
    tools
}

/// #202: [`builtin_tools_with_shell_and_jobs`] with the bash tool running through `sandbox`
/// (the session's full-access wrapper) instead of `ToolCtx::sandbox`.
pub fn builtin_tools_with_bash_sandbox(
    shell: ShellConfig,
    jobs: Arc<JobRegistry>,
    sandbox: Arc<dyn oxa_proto::Sandbox>,
) -> Vec<Arc<dyn Tool>> {
    let mut tools = builtin_tools();
    tools[1] = Arc::new(BashTool::with_shell_and_jobs(shell, jobs).with_sandbox(sandbox));
    tools
}

/// Resolve a tool path argument against `cwd` and normalise it lexically (`.` and `..` removed
/// without touching the file system). Shared by every tool's `access` and `run`.
pub fn resolve_path(cwd: &Path, arg: &str) -> PathBuf {
    let joined = if Path::new(arg).is_absolute() {
        PathBuf::from(arg)
    } else {
        cwd.join(arg)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// #597: the same path identity for every file tool's permission check and execution.
/// Explicit absolute `/tmp` paths name session scratch, except host mounts and outputs.
/// Relative paths retain their host-cwd meaning, including `..` outside the workspace.
/// This translates names only: the policy still judges the resolved host path.
/// #955: the note a file tool adds to a success message when the file it wrote is hidden from
/// the sandboxed bash tool (warn, never refuse). Nothing for a path bash shares (read-only or
/// not) and nothing under the session's `/tmp`, which bash sees as `/tmp`.
pub(crate) fn bash_view_note(cx: &oxa_proto::ToolCtx, path: &Path) -> Option<String> {
    if cx.sandbox.scratch_dir().is_some_and(|sc| path.starts_with(sc)) {
        return None;
    }
    (cx.sandbox.bash_view(cx.cwd, path, true) == oxa_proto::BashView::Hidden).then(|| {
        "Note: sandboxed bash cannot see this path (it is hidden there). Add it to oxa.sandbox.readPaths or oxa.sandbox.writePaths in user settings to share it.".to_string()
    })
}

pub fn resolve_tool_path(cx: &oxa_proto::ToolCtx, arg: &str) -> PathBuf {
    let abs = resolve_path(cx.cwd, arg);
    let Ok(rest) = abs.strip_prefix("/tmp") else {
        return abs;
    };
    let Some(scratch) = cx.sandbox.file_scratch_dir(cx.mode) else {
        return abs;
    };
    if !Path::new(arg).is_absolute()
        || abs.starts_with(cx.cwd)
        || std::fs::canonicalize(cx.cwd).is_ok_and(|real| abs.starts_with(real))
        || abs.starts_with(cx.spill_dir)
        || abs.starts_with(scratch)
        || cx.sandbox.preserves_host_path(&abs)
    {
        return abs;
    }
    scratch.join(rest)
}
