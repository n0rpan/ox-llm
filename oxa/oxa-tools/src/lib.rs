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

mod bash;
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

pub use bash::{net_for as bash_net_for, BashTool, ShellConfig};
pub use diff::{apply_edits, detect_line_ending, strip_bom, unified_diff, Edit, EditOutcome};
pub use edit::EditTool;
pub use find::FindTool;
pub use grep::GrepTool;
pub use jobs::{JobRead, JobRegistry, JobStatus, BUFFER_CAP, MAX_JOBS, READ_CAP};
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
