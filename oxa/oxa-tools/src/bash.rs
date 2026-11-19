//! A9: the `bash` tool.
// ported from pi packages/coding-agent/src/core/tools/bash.ts (execution replaced by the sandbox)
use crate::descriptions::{self, errors};
use crate::jobs::{JobRegistry, JobStatus};
use oxa_proto::{sh_quote, Access, ExecSpec, Mode, Sandbox, Tool, ToolCtx, ToolOutput, ToolSpec, Value};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// `bash {command, timeout?}`: runs through `ToolCtx::sandbox` (never directly). `writable` = mode is not Plan; `net` = [`net_for`] (O-167(b): the sandbox network by mode and `oxa.sandbox.net`; `access` keeps `net: false`). Output streams to `live`; the result text is the merged output plus pi's exit-code / timeout lines; no timing text (W4). If `sandbox.check()` fails the result is that error (`is_error`). `ToolOutput::ui` = `{"exit": n|null, "timedOut": bool}`. Environment: `ToolCtx::env`.
#[derive(Debug, Clone)]
pub struct BashTool {
    spec: ToolSpec,
    shell: ShellConfig,
    jobs: Arc<JobRegistry>,
    /// #202: the sandbox the tool runs through instead of `ToolCtx::sandbox` (the session's
    /// `oxa_sandbox::FullAccess`, which drops bwrap under `yolo`). `None` = `ToolCtx::sandbox`.
    sandbox: Option<OwnSandbox>,
}

/// An own sandbox of the tool (no `Debug` on `dyn Sandbox`).
#[derive(Clone)]
struct OwnSandbox(Arc<dyn Sandbox>);

impl std::fmt::Debug for OwnSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OwnSandbox")
    }
}

impl BashTool {
    pub fn new() -> BashTool {
        BashTool {
            spec: crate::spec_of(&descriptions::builtin_specs::BASH_DEFAULT),
            shell: ShellConfig::default(),
            jobs: Arc::new(JobRegistry::new()),
            sandbox: None,
        }
    }
    /// H: the tool with pi's `shellPath` / `shellCommandPrefix` settings.
    pub fn with_shell(shell: ShellConfig) -> BashTool {
        BashTool {
            spec: crate::spec_of(&descriptions::builtin_specs::BASH_DEFAULT),
            shell,
            jobs: Arc::new(JobRegistry::new()),
            sandbox: None,
        }
    }
    /// Issue #127 (b): the tool with `shell` and a shared [`JobRegistry`] (the session's).
    pub fn with_shell_and_jobs(shell: ShellConfig, jobs: Arc<JobRegistry>) -> BashTool {
        BashTool {
            spec: crate::spec_of(&descriptions::builtin_specs::BASH_DEFAULT),
            shell,
            jobs,
            sandbox: None,
        }
    }
    /// #202: run through `sandbox` instead of `ToolCtx::sandbox`.
    pub fn with_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> BashTool {
        self.sandbox = Some(OwnSandbox(sandbox));
        self
    }
    fn sandbox<'a>(&'a self, cx: &'a ToolCtx) -> &'a dyn Sandbox {
        match &self.sandbox {
            Some(OwnSandbox(s)) => s.as_ref(),
            None => cx.sandbox,
        }
    }
}

/// H: pi's `shellPath` and `shellCommandPrefix` settings (settings.md "Shell"), shared by the
/// `bash` tool and the user's `!cmd`. The sandbox always runs `bash -c <text>`; this builds
/// that text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellConfig {
    /// The custom shell executable (`shellPath`, `~` already expanded). It must be visible
    /// inside the sandbox: `/` is read-only there, home and `/tmp` are hidden.
    pub shell_path: Option<PathBuf>,
    /// `shellCommandPrefix`, prepended to every command on its own line (pi bash.ts).
    pub command_prefix: Option<String>,
}

impl ShellConfig {
    /// The text the sandbox runs for `command`: pi's `${prefix}\n${command}`, and with a
    /// `shell_path` `exec '<shell>' -c '<that>'`. `Err` is pi's text for a missing shell
    /// (`Custom shell path not found: <path>`, utils/shell.ts).
    pub fn command(&self, command: &str) -> Result<String, String> {
        let resolved = match self.command_prefix.as_deref().filter(|p| !p.is_empty()) {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command.to_string(),
        };
        match &self.shell_path {
            None => Ok(resolved),
            Some(shell) => {
                if !shell.exists() {
                    return Err(format!("Custom shell path not found: {}", shell.display()));
                }
                Ok(format!(
                    "exec {} -c {}",
                    sh_quote(&shell.to_string_lossy()),
                    sh_quote(&resolved)
                ))
            }
        }
    }
}

/// Cheap heuristic: some line of the command ends with a background `&` (not `&&`), or the command uses `nohup`.
pub(crate) fn looks_backgrounded(command: &str) -> bool {
    let backgrounded_line = command.lines().any(|line| {
        let t = line.trim_end();
        t.ends_with('&') && !t.ends_with("&&")
    });
    backgrounded_line || command.split_whitespace().any(|w| w == "nohup")
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

/// The validated arguments of one call.
#[derive(Debug, Clone, PartialEq)]
enum BashArgs {
    /// Run `command` (foreground, or detached when `background`).
    Run {
        command: String,
        /// Whole seconds, rounded up; `None` = no timeout.
        timeout_s: Option<u32>,
        background: bool,
    },
    /// Read the status and output of a background job, after waiting up to `wait` (#150).
    /// With `kill`, stop the job instead of reading it (#519).
    Job {
        id: String,
        wait: Option<Duration>,
        kill: bool,
    },
}

/// #150 (2): the longest `wait_s` a job read blocks for; a larger value is cut to it.
pub const WAIT_CAP_S: f64 = 60.0;
/// #618: keep PID isolation; explain why a separate process listing cannot inspect a job.
const JOB_PROCESS_VISIBILITY: &str = "Each bash command runs in a separate PID namespace; ps in another bash call cannot see this job. Use the job interface for status and captured output, not cross-call process inspection.";
const BAD_WAIT: &str = "\"wait_s\" must be a number of seconds, zero or more";
const WAIT_NEEDS_JOB: &str = "\"wait_s\" applies to a job read; provide \"job\" with it";

/// `wait_s` of a call: absent, null or zero is no wait; otherwise capped at [`WAIT_CAP_S`].
/// Negative, non-finite or non-numeric values are refused.
fn parse_wait(args: &Value) -> Result<Option<Duration>, String> {
    match args.get("wait_s") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let w = v
                .as_f64()
                .filter(|w| w.is_finite() && *w >= 0.0)
                .ok_or_else(|| BAD_WAIT.to_string())?;
            if w == 0.0 {
                return Ok(None);
            }
            Ok(Some(Duration::from_secs_f64(w.min(WAIT_CAP_S))))
        }
    }
}

/// pi's validation text for a missing required property (TypeBox's message, verbatim shape).
const MISSING_COMMAND: &str =
    "Validation failed for tool \"bash\":\n  - must have required property 'command'";

/// Issue #127 (b): exactly one of `command` / `job` must be given.
const COMMAND_OR_JOB: &str =
    "Provide exactly one of \"command\" (to run) or \"job\" (to read a background job)";
const KILL_NEEDS_JOB: &str = "\"kill\" applies to a job; provide \"job\", not \"command\"";
const BACKGROUND_NEEDS_COMMAND: &str =
    "\"background\" applies to a command; provide \"command\", not \"job\"";

fn parse(args: &Value) -> Result<BashArgs, String> {
    let command = args.get("command").and_then(Value::as_str);
    let job = args.get("job").and_then(Value::as_str);
    let background = args.get("background").and_then(Value::as_bool).unwrap_or(false);

    let kill = args.get("kill").and_then(Value::as_bool).unwrap_or(false);

    match (command, job) {
        (Some(_), Some(_)) => Err(COMMAND_OR_JOB.to_string()),
        (None, None) => Err(MISSING_COMMAND.to_string()),
        (Some(command), None) => {
            if args.get("wait_s").is_some_and(|v| !v.is_null()) {
                return Err(WAIT_NEEDS_JOB.to_string());
            }
            if kill {
                return Err(KILL_NEEDS_JOB.to_string());
            }
            let timeout_s = match args.get("timeout") {
                None | Some(Value::Null) => None,
                Some(v) => {
                    // pi: a finite number of seconds greater than zero, else refused.
                    let t = v
                        .as_f64()
                        .filter(|t| t.is_finite() && *t > 0.0)
                        .ok_or_else(|| errors::BASH_BAD_TIMEOUT.to_string())?;
                    Some(t.ceil().min(f64::from(u32::MAX)) as u32)
                }
            };
            Ok(BashArgs::Run {
                command: command.to_string(),
                timeout_s,
                background,
            })
        }
        (None, Some(id)) => {
            if background {
                return Err(BACKGROUND_NEEDS_COMMAND.to_string());
            }
            Ok(BashArgs::Job {
                id: id.to_string(),
                wait: parse_wait(args)?,
                kill,
            })
        }
    }
}

/// Split off the longest prefix of `pending` that is valid UTF-8 (keeping an incomplete trailing
/// sequence for the next chunk); invalid bytes become U+FFFD.
fn take_valid_utf8(pending: &mut Vec<u8>) -> String {
    let mut out = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(s) => {
                out.push_str(s);
                pending.clear();
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                out.push_str(&String::from_utf8_lossy(&pending[..valid]));
                match e.error_len() {
                    Some(n) => {
                        out.push('\u{FFFD}');
                        pending.drain(..valid + n);
                    }
                    None => {
                        pending.drain(..valid);
                        break;
                    }
                }
            }
        }
    }
    out
}

/// TOOL-DIET for the `bash` result text (`ToolTune::diet`): each run of three or more blank
/// lines (empty or whitespace-only) is cut to its first two. Nothing else changes: every other
/// line keeps its bytes, trailing blanks included (a diff's context lines and a file shown with
/// `cat` stay copyable), and a run of one or two blank lines stays as it is. Deterministic.
pub(crate) fn diet_output(text: &str) -> String {
    let (body, tail) = match text.strip_suffix('\n') {
        Some(body) => (body, "\n"),
        None => (text, ""),
    };
    let mut out: Vec<&str> = Vec::new();
    let mut blanks = 0usize;
    for line in body.split('\n') {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 2 {
                continue;
            }
        } else {
            blanks = 0;
        }
        out.push(line);
    }
    format!("{}{tail}", out.join("\n"))
}

/// A hedged note for `No such file or directory` on a path under the real home that is outside
/// the workspace. The sandbox may hide unbound home paths, but a missing file under a mounted
/// directory (a read-only toolchain bind, a configured bind) is simply absent, so the note never
/// claims the path is unmounted; it tells the reader to check the parent directory first.
/// Only the workspace (`cwd`) counts as visible: neither `ToolCtx` nor `Sandbox` exposes the
/// bound/extra paths, so no mount introspection is done here.
pub(crate) fn unmounted_home_note(output: &str, home: &str, cwd: &Path) -> Option<String> {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return None;
    }
    let prefix = format!("{home}/");
    // A command's own error line ends with the message; the same words in the middle of a line
    // are quoted text (an issue body, a log) and must not trigger the note.
    for line in output
        .lines()
        .filter(|l| l.trim_end().ends_with("No such file or directory"))
    {
        for tok in line.split(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '`' | ':')) {
            if tok.starts_with(&prefix) && !Path::new(tok).starts_with(cwd) {
                return Some(format!(
                    "{tok}: not found. It may be hidden because the path is outside the sandbox mounts (only the workspace and listed binds are visible), or it may simply not exist; check whether its parent directory exists before concluding it is unmounted"
                ));
            }
        }
    }
    None
}

/// #494: git keeps `.git/config` read-only in the sandbox (a writable config could run code
/// outside it), so a command that must rewrite it fails with "Device or resource busy" after
/// it has done the rest, e.g. `git branch -m` renames the ref first. Say so. Only a failed
/// command with git's own error line counts (it starts with `error: could not write config
/// file`); the same words quoted in an issue body or a successful command's log do not.
/// The note names the file from that line: the repository config gets the "branch was renamed"
/// wording, any other config (for example the global `~/.gitconfig`) a plain one.
pub(crate) fn git_config_note(output: &str, exit: Option<i32>) -> Option<String> {
    if exit == Some(0) {
        return None;
    }
    const PREFIX: &str = "error: could not write config file ";
    let rest = output.lines().find_map(|l| l.trim_end().strip_prefix(PREFIX))?;
    // `<path>: <strerror>`; the path may itself contain ": " only in odd cases, so split at the last one.
    let path = rest.rsplit_once(": ").map_or(rest, |(p, _)| p).trim();
    if path.is_empty() {
        return None;
    }
    if path == ".git/config" || path.ends_with("/.git/config") || path.contains("/.git/") {
        Some(format!("git could not write {path}: it is read-only in the sandbox, so anything git did before that step (for example the branch was renamed) stays done, and only the config update (branch upstream settings, remotes) is missing. Check the result with `git branch`; the config can be edited outside the sandbox"))
    } else {
        Some(format!("git could not write {path}: it is not writable in the sandbox, so that config was not changed. Edit it outside the sandbox"))
    }
}

/// #290: keep shared git metadata protected, but offer an exclusion route that needs no
/// tracked-file edits. Only a failed shell redirection diagnostic counts, not quoted issue text.
pub(crate) fn git_exclude_note(output: &str, exit: Option<i32>) -> Option<String> {
    if !exit.is_some_and(|code| code != 0) {
        return None;
    }
    output.lines().find(|line| {
        line.starts_with("bash: ") && line.trim_end().ends_with("/info/exclude: Read-only file system")
    })?;
    Some("Git's info/exclude is shared git metadata and stays read-only in a worktree sandbox. To ignore a local scratch directory without changing tracked files, put a .gitignore inside that directory, for example `printf '*\\n' > .venv-test/.gitignore` (the directory must already exist). This ignores its contents, including that .gitignore; do not overwrite an existing tracked .gitignore. For individual files, use `git -c core.excludesFile=<local-ignore-file> status` with patterns in a writable local file; that override applies only to that git invocation".to_string())
}

/// #253: when stdout is not a terminal, `gh issue view --comments` (also `gh pr view`) prints the
/// comments only, so an issue without comments gives an empty, successful result that reads as a
/// failure of the sandbox. Say what happened and how to get the text. Only for a successful
/// command whose output is empty (tool warnings such as mise's `WARN` lines do not count).
pub(crate) fn gh_comments_note(command: &str, output: &str, exit: Option<i32>) -> Option<&'static str> {
    // Redirects and pipelines can hide real comments. Skip both rather than guessing
    // whether later stages preserve output (even `head` can discard it with `-n 0`).
    // `2>&1` only merges stderr into captured stdout and does not hide any comments.
    if command.replace("2>&1", "").contains('>') || command.contains('|') {
        return None;
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    let viewing = words
        .windows(3)
        .any(|w| w[0] == "gh" && matches!(w[1], "issue" | "pr") && w[2] == "view");
    let comments = words.iter().any(|w| *w == "--comments" || *w == "-c");
    let json = words
        .iter()
        .any(|w| w.starts_with("--json") || w.starts_with("--jq") || *w == "-q");
    let silent = output
        .lines()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with("mise WARN"));
    (exit == Some(0) && viewing && comments && !json && silent).then_some("gh prints only the comments when its output is captured, so an issue or pull request without comments gives no output; the issue text is not missing. Use `gh issue view N -R OWNER/REPO --json title,body,comments,state` (or `gh pr view`) to get the body and the comments")
}

/// #670: a detached tmux server cannot outlive the sandbox command that owns its PID namespace.
/// Do not diagnose successful commands or quoted errors from unrelated tools.
fn tmux_server_note(command: &str, output: &str, exit: Option<i32>) -> Option<&'static str> {
    (exit.is_some_and(|code| code != 0)
        && command.split_whitespace().any(|word| word == "tmux")
        && output.lines().any(|line| line.starts_with("no server running")))
    .then_some(descriptions::BASH_PROCESS_LIFETIME)
}

/// O-167(b): the network of a bash command. `sandbox_net` is the sandbox's answer
/// (`oxa.sandbox.net`, default on, and never on while offline). Per mode: plan = never (a
/// read-only mode does not gain the network silently); workspace = `sandbox_net`; ask =
/// `sandbox_net` (the user approves every command). The model cannot change it: it is not an
/// argument and not an `Access` grant (`access` keeps `net: false`, so the policy table and
/// its prompts are unchanged).
pub fn net_for(mode: Mode, sandbox_net: bool) -> bool {
    sandbox_net && !matches!(mode, Mode::Plan)
}

impl Tool for BashTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// `[Access::Exec { command, net: false }]` for a run; nothing for a job read.
    fn access(&self, args: &Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        match parse(args)? {
            BashArgs::Run { command, .. } => Ok(vec![Access::Exec { command, net: false }]),
            BashArgs::Job { .. } => Ok(Vec::new()),
        }
    }
    fn run(&self, args: &Value, cx: &ToolCtx, live: &mut dyn FnMut(&str)) -> ToolOutput {
        let a = match parse(args) {
            Ok(a) => a,
            Err(e) => return ToolOutput::error(e),
        };
        match a {
            BashArgs::Job { id, kill: true, .. } => self.kill_job(&id),
            BashArgs::Job { id, wait, .. } => self.read_job(&id, wait, cx),
            BashArgs::Run {
                command,
                timeout_s,
                background,
            } => {
                if background {
                    return self.start_job(&command, timeout_s, cx);
                }
                self.run_foreground(&command, timeout_s, cx, live)
            }
        }
    }
}

impl BashTool {
    /// Issue #127 (b): read a background job's status and the last 8 KiB of its output.
    /// #150 (2): with `wait`, first block until the job exits, prints new output, or the wait
    /// runs out. A still-running job's text says to wait again later rather than retry at once.
    fn read_job(&self, id: &str, wait: Option<Duration>, cx: &ToolCtx) -> ToolOutput {
        if let Some(max) = wait {
            if let Err(e) = self.jobs.wait(id, max, cx.cancel) {
                return ToolOutput::error(e);
            }
        }
        match self.jobs.read(id) {
            Ok(read) => {
                let status = read.status.text();
                let mut text = format!("Job {id}: {status} ({} bytes of output)", read.total);
                if read.status == JobStatus::Running {
                    text.push_str(&format!(
                        "\nStill running. Wait with bash {{\"job\": \"{id}\", \"wait_s\": 30}} or poll again later; do not retry at once."
                    ));
                }
                if !read.output.is_empty() {
                    text.push_str("\n\n");
                    text.push_str(&read.output);
                }
                ToolOutput {
                    images: Vec::new(),
                    text,
                    is_error: false,
                    ui: json!({ "job": id, "status": status }),
                }
            }
            Err(e) => ToolOutput::error(e),
        }
    }

    /// #519: stop one background job and free its slot.
    fn kill_job(&self, id: &str) -> ToolOutput {
        match self.jobs.kill(id) {
            Ok(k) => {
                let status = k.status.text();
                let mut text = format!("Job {id}: {status}");
                if !k.was_running {
                    text.push_str(" (already finished)");
                }
                ToolOutput {
                    images: Vec::new(),
                    text,
                    is_error: false,
                    ui: json!({ "job": id, "status": status }),
                }
            }
            Err(e) => ToolOutput::error(e),
        }
    }

    /// Issue #127 (b): start `command` detached and return its job id at once.
    fn start_job(&self, command: &str, timeout_s: Option<u32>, cx: &ToolCtx) -> ToolOutput {
        if let Err(e) = self.sandbox(cx).check() {
            return ToolOutput::error(e.to_string());
        }
        let command = match self.shell.command(command) {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(e),
        };
        let spec = ExecSpec {
            command,
            cwd: cx.cwd.to_path_buf(),
            writable: !matches!(cx.mode, Mode::Plan),
            net: net_for(cx.mode, self.sandbox(cx).network()),
            // A background job has no timeout unless the call names one: it runs until it
            // exits, is killed (`bash {job, kill: true}`) or the session ends.
            timeout_s,
            env: cx.env.to_vec(),
        };
        match self.jobs.spawn(spec) {
            Ok(id) => ToolOutput {
                images: Vec::new(),
                text: format!(
                    "Started background job {id}. Read its status and output with bash {{\"job\": \"{id}\"}}.\n{JOB_PROCESS_VISIBILITY}"
                ),
                is_error: false,
                ui: json!({ "job": id }),
            },
            Err(e) => ToolOutput::error(e),
        }
    }

    /// The existing foreground path, unchanged except that the "background processes were
    /// stopped" notice is never printed for a `background: true` start (which never reaches
    /// here).
    fn run_foreground(
        &self,
        command: &str,
        timeout_s: Option<u32>,
        cx: &ToolCtx,
        live: &mut dyn FnMut(&str),
    ) -> ToolOutput {
        if let Err(e) = self.sandbox(cx).check() {
            return ToolOutput::error(e.to_string());
        }
        let backgrounded = looks_backgrounded(command);
        let raw_command = command;
        let command = match self.shell.command(command) {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(e),
        };
        let spec = ExecSpec {
            command,
            cwd: cx.cwd.to_path_buf(),
            writable: !matches!(cx.mode, Mode::Plan),
            net: net_for(cx.mode, self.sandbox(cx).network()),
            // F5: the call's own timeout wins; else the epoch's default (0 = none, pi).
            timeout_s: timeout_s.or_else(|| (cx.tune.bash_timeout_s > 0).then_some(cx.tune.bash_timeout_s)),
            env: cx.env.to_vec(),
        };
        let mut pending: Vec<u8> = Vec::new();
        let mut sink = |chunk: &[u8]| {
            pending.extend_from_slice(chunk);
            let text = take_valid_utf8(&mut pending);
            if !text.is_empty() {
                live(&text);
            }
        };
        let res = self.sandbox(cx).run(&spec, cx.cancel, &mut sink);
        if !pending.is_empty() {
            live(&String::from_utf8_lossy(&pending));
        }

        let mut output = String::from_utf8_lossy(&res.output).into_owned();
        // Successful reads/mentions stay quiet; scratch writes and relevant failures get a notice.
        let persistence_relevant = scratch_write_likely(raw_command)
            || (res.exit != Some(0) && (raw_command.contains("/tmp") || raw_command.contains("TMPDIR")))
            || (res.exit != Some(0)
                && [
                    "read-only file system",
                    "permission denied",
                    "operation not permitted",
                ]
                .iter()
                .any(|error| output.to_ascii_lowercase().contains(error)));
        if let Some(notice) = self
            .sandbox(cx)
            .persistence_notice()
            .filter(|_| persistence_relevant)
        {
            output.push_str("\n\n");
            output.push_str(notice);
        }
        // TOOL-DIET: the result text only; the raw bytes went to `live` above.
        let output = if cx.tune.diet {
            diet_output(&output)
        } else {
            output
        };
        // A trailing `&` or `nohup` means the shell's children were cut off at the end of the call.
        let output = if backgrounded {
            if output.is_empty() {
                errors::BASH_BACKGROUND_STOPPED.to_string()
            } else {
                format!("{output}\n\n{}", errors::BASH_BACKGROUND_STOPPED)
            }
        } else {
            output
        };
        let mut ui = json!({ "exit": res.exit, "timedOut": res.timed_out });
        if res.unsandboxed {
            // Front ends warn on this; absent (not `false`) everywhere the OS sandbox ran.
            ui["unsandboxed"] = json!(true);
        }
        let note = cx
            .sandbox
            .home()
            .and_then(|h| unmounted_home_note(&output, &h.to_string_lossy(), cx.cwd));
        let note = note.or_else(|| git_config_note(&output, res.exit));
        let note = note.or_else(|| git_exclude_note(&output, res.exit));
        let note = note.or_else(|| gh_comments_note(raw_command, &output, res.exit).map(str::to_string));
        let note = note.or_else(|| tmux_server_note(raw_command, &output, res.exit).map(str::to_string));
        let shown = if output.is_empty() {
            match note {
                Some(n) if res.exit == Some(0) => format!("{}\n[{n}]", errors::BASH_NO_OUTPUT),
                _ => errors::BASH_NO_OUTPUT.to_string(),
            }
        } else if let Some(n) = note {
            format!("{}\n[{n}]", output.trim_end())
        } else {
            output.clone()
        };

        if res.timed_out {
            let secs = spec.timeout_s.unwrap_or(0);
            return ToolOutput {
                images: Vec::new(),
                text: errors::bash_with_status(&output, &errors::bash_timed_out(secs)),
                is_error: true,
                ui,
            };
        }
        if cx.cancel.is_cancelled() {
            return ToolOutput {
                images: Vec::new(),
                text: errors::bash_with_status(&output, errors::BASH_ABORTED),
                is_error: true,
                ui,
            };
        }
        match res.exit {
            None => ToolOutput {
                images: Vec::new(),
                text: errors::bash_with_status(&shown, errors::BASH_NO_EXIT_CODE),
                is_error: true,
                ui,
            },
            Some(0) => ToolOutput {
                images: Vec::new(),
                text: shown,
                is_error: false,
                ui,
            },
            Some(code) => ToolOutput {
                images: Vec::new(),
                text: errors::bash_with_status(&shown, &errors::bash_exit(code)),
                is_error: true,
                ui,
            },
        }
    }
}

/// Suppress the successful-command hint only for recognizable reads. Enumerating writers
/// misses interpreters, build tools, attached output flags and custom scripts. This is a
/// conservative hint heuristic, NOT a shell parser or a permission check; unknown syntax
/// keeps the hint. Failed commands mentioning scratch always get it, regardless of syntax.
fn scratch_write_likely(command: &str) -> bool {
    if !(command.contains("/tmp") || command.contains("TMPDIR")) {
        return false;
    }
    // Substitutions can write even inside a read's arguments.
    if command.contains("$(") || command.contains('`') {
        return true;
    }
    // Keep quoted/escaped shell words together so grep '->' is not a redirect and quoted
    // separators do not invent commands. Recognize output redirects separately, including
    // fd duplication (2>&1), which does not create a file.
    static TOKENS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let tokens = TOKENS.get_or_init(|| {
        regex::Regex::new(
            r#"(?s)(?:[0-9]*>>?|&>>?)(?:&[0-9-]+)?|[;&|()<\n]|(?:[^\s'"\\;&|()<>]+|'[^']*'|"(?:\\.|[^"\\])*"|\\.)+"#,
        )
        .expect("shell hint tokens")
    });
    const READERS: &[&str] = &[
        "echo", "printf", "ls", "cat", "grep", "head", "tail", "wc", "stat", "pwd", "test", "[", "true",
        "false", "readlink", "basename", "dirname",
    ];
    let mut words = tokens.find_iter(command).map(|m| m.as_str());
    let mut command_start = true;
    while let Some(word) = words.next() {
        if matches!(word, ";" | "&" | "|" | "\n") {
            command_start = true;
            continue;
        }
        let op = word.trim_start_matches(|c: char| c.is_ascii_digit());
        if op.starts_with(">&") {
            continue;
        }
        if op.starts_with('>') || op.starts_with("&>") {
            if words.next().map(|w| w.trim_matches(['\'', '"'])) == Some("/dev/null") {
                continue;
            }
            return true;
        }
        if matches!(word, "(" | ")" | "<") {
            return true;
        }
        if command_start {
            let name = word.trim_matches(['\'', '"']).rsplit('/').next().unwrap_or(word);
            if !READERS.contains(&name) {
                return true;
            }
            command_start = false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #532: a successful read that merely mentions /tmp gets no persistence notice.
    #[test]
    fn scratch_notice_only_for_writes() {
        for c in [
            "grep -n '\"/tmp\"' src/lib.rs",
            "ls /tmp 2>&1",
            "cat /tmp/x >/dev/null",
            "echo $TMPDIR",
            "grep -o x /tmp/f",
            "grep '->' /tmp/f",
            "grep 'cp; ->' /tmp/f",
            "echo 'tee' /tmp",
            "ls /tmp >'/dev/null' 2>&1",
            "printf '%s\\n' '->' /tmp/f",
            "ls src",
        ] {
            assert!(!scratch_write_likely(c), "{c}");
        }
    }

    #[test]
    fn scratch_notice_covers_common_writes() {
        for c in [
            "echo x > /tmp/f",
            "echo x >>/tmp/f",
            "cmd &> /tmp/log",
            "mktemp -p /tmp",
            "cp a /tmp/a",
            "ls | tee /tmp/l",
            "touch $TMPDIR/x",
            "cd /tmp && make",
            "cd $TMPDIR && npm install",
            "sed -i 's/x/y/' /tmp/f",
            "python -c \"open('/tmp/x','w').write('x')\"",
            "gcc -o/tmp/a input.c",
            "sort -o/tmp/x input.txt",
            "custom-script /tmp/output",
            "echo $(touch /tmp/f)",
            "echo `touch /tmp/f`",
            "cat /tmp/f\ntouch /tmp/g",
        ] {
            assert!(scratch_write_likely(c), "{c}");
        }
    }

    /// H (SET-055, SET-056): pi's `${prefix}\n${command}`, the custom shell through `exec`, and
    /// pi's error for a missing shell.
    #[test]
    fn shell_config_builds_the_sandboxed_command() {
        let plain = ShellConfig::default();
        assert_eq!(plain.command("echo hi").unwrap(), "echo hi");
        let prefixed = ShellConfig {
            shell_path: None,
            command_prefix: Some("shopt -s expand_aliases".to_string()),
        };
        assert_eq!(prefixed.command("ll").unwrap(), "shopt -s expand_aliases\nll");
        let shell = ShellConfig {
            shell_path: Some(PathBuf::from("/bin/sh")),
            command_prefix: Some("X=1".to_string()),
        };
        assert_eq!(
            shell.command("echo 'a'").unwrap(),
            r#"exec '/bin/sh' -c 'X=1
echo '\''a'\'''"#
        );
        let missing = ShellConfig {
            shell_path: Some(PathBuf::from("/nonexistent/shell-h")),
            command_prefix: None,
        };
        assert_eq!(
            missing.command("x").unwrap_err(),
            "Custom shell path not found: /nonexistent/shell-h"
        );
    }
    use oxa_proto::{CancelToken, ExecResult, Mode, OxaError, Sandbox};
    use std::path::Path;
    use std::sync::Mutex;

    /// A canned sandbox: `check` result, a fixed `run` result, streamed chunks, and the spec seen.
    struct Fake {
        check: Option<OxaError>,
        result: ExecResult,
        chunks: Vec<Vec<u8>>,
        seen: Mutex<Option<ExecSpec>>,
        home: Option<PathBuf>,
    }

    impl Fake {
        fn ok(output: &[u8], exit: Option<i32>) -> Fake {
            Fake {
                check: None,
                result: ExecResult {
                    output: output.to_vec(),
                    exit,
                    timed_out: false,
                    unsandboxed: false,
                },
                chunks: vec![output.to_vec()],
                seen: Mutex::new(None),
                home: None,
            }
        }
    }

    impl Sandbox for Fake {
        fn check(&self) -> Result<(), OxaError> {
            match &self.check {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
        fn home(&self) -> Option<&Path> {
            self.home.as_deref()
        }
        fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, s: &ExecSpec, _c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
            *self.seen.lock().unwrap() = Some(s.clone());
            for c in &self.chunks {
                live(c);
            }
            self.result.clone()
        }
    }

    fn run_with(sb: &Fake, args: Value, mode: Mode, cancel: &CancelToken) -> (ToolOutput, String) {
        let cwd = Path::new("/work");
        let env = vec![("OXA_SESSION_ID".to_string(), "s".to_string())];
        let cx = ToolCtx {
            checked: &[],
            cwd,
            spill_dir: cwd,
            sandbox: sb,
            mode,
            cancel,
            env: &env,
            tune: Default::default(),
            hidden: None,
        };
        let mut streamed = String::new();
        let out = BashTool::new().run(&args, &cx, &mut |t| streamed.push_str(t));
        (out, streamed)
    }

    /// #670: explain a vanished detached server without relaxing process isolation.
    #[test]
    fn tmux_missing_server_explains_sandbox_lifetime() {
        let sb = Fake::ok(b"no server running on /tmp/live.sock\n", Some(1));
        let (out, _) = run_with(
            &sb,
            json!({"command": "tmux -S /tmp/live.sock capture-pane -p -t live"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert!(out.text.contains("no server running on /tmp/live.sock"));
        assert!(out.text.contains("PID namespace"), "{}", out.text);
        assert!(out.text.contains("background: true"), "{}", out.text);
        assert!(out.text.contains("keep the command alive"), "{}", out.text);
    }

    #[test]
    fn tmux_lifetime_guidance_is_available_before_starting_a_server() {
        let guidance = descriptions::BASH.guidelines.join("\n");
        assert!(guidance.contains("tmux"));
        assert!(guidance.contains("PID namespace"));
        assert!(guidance.contains("keep the command alive"));
    }

    #[test]
    fn tmux_server_note_does_not_diagnose_unrelated_failures() {
        let missing = "no server running on /tmp/live.sock\n";
        assert!(tmux_server_note("tmux capture-pane", missing, Some(0)).is_none());
        assert!(tmux_server_note("tmux capture-pane", missing, None).is_none());
        assert!(tmux_server_note("cat log.txt", missing, Some(1)).is_none());
        assert!(tmux_server_note("tmux bad-command", "unknown command: bad-command", Some(1)).is_none());
        assert!(tmux_server_note("tmux capture-pane", "old error: no server running", Some(1)).is_none());
    }

    /// #526: the note uses the sandbox's home, never the process `$HOME`.
    #[test]
    fn unmounted_home_note_follows_the_sandbox_home() {
        let msg = b"cat: /home/sess/other/x: No such file or directory\n";
        let mut sb = Fake::ok(msg, Some(1));
        sb.home = Some(PathBuf::from("/home/sess"));
        let (out, _) = run_with(
            &sb,
            json!({"command": "cat /home/sess/other/x"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(
            out.text.contains("/home/sess/other/x: not found."),
            "{}",
            out.text
        );
        // A sandbox that does not know its home adds no note, whatever the process `$HOME` is.
        let sb = Fake::ok(msg, Some(1));
        let (out, _) = run_with(
            &sb,
            json!({"command": "cat /home/sess/other/x"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(!out.text.contains("not found."), "{}", out.text);
    }

    #[test]
    fn note_for_unmounted_home_path_only() {
        let cwd = Path::new("/home/u/proj");
        let msg = "touch: cannot touch '/home/u/oxc/probe': No such file or directory\n";
        let note = unmounted_home_note(msg, "/home/u", cwd).expect("note");
        assert!(note.contains("/home/u/oxc/probe: not found."));
        assert!(note.contains("may"));
        assert!(note.contains("parent directory"));
        assert!(!note.contains("path outside the sandbox (not mounted)"));
        let mounted_parent = "ls: cannot access '/home/u/.cargo/bin/nothing': No such file or directory\n";
        let hedged = unmounted_home_note(mounted_parent, "/home/u", cwd).expect("note");
        assert!(hedged.contains("may"));
        assert!(hedged.contains("parent directory"));
        assert!(!hedged.contains("path outside the sandbox (not mounted)"));
        let inside = "cat: /home/u/proj/x: No such file or directory\n";
        assert!(unmounted_home_note(inside, "/home/u", cwd).is_none());
        let other = "cat: /etc/x: No such file or directory\n";
        assert!(unmounted_home_note(other, "/home/u", cwd).is_none());
        assert!(unmounted_home_note("/home/u/oxc/p: Permission denied", "/home/u", cwd).is_none());
    }

    #[test]
    fn no_note_for_quoted_text_that_is_not_a_failed_command() {
        let cwd = Path::new("/home/u/proj");
        // GitHub issue JSON that quotes an old error: the message is mid-line, not a command's own line.
        let json = r#"{"body":"it failed: /home/u/.venv/bin/python: No such file or directory\nthen we retried","comments":[]}"#;
        assert!(unmounted_home_note(json, "/home/u", cwd).is_none());
        let prose = "The old error was /home/u/.venv/bin/python: No such file or directory when run.\n";
        assert!(unmounted_home_note(prose, "/home/u", cwd).is_none());
        // A real error line, also with a program prefix and trailing whitespace, still gets the note.
        let real = "bash: /home/u/.venv/bin/python: No such file or directory  \n";
        assert!(unmounted_home_note(real, "/home/u", cwd).is_some());
    }

    #[test]
    fn git_config_note_needs_git_error_line_and_names_the_file() {
        // Quoted text (an issue body, a log) is not a failed git command.
        assert!(git_config_note(
            "the log says error: could not write config file .git/config: x\n",
            Some(1),
        )
        .is_none());
        assert!(git_config_note(
            r#"{"body":"error: could not write config file .git/config: Device or resource busy"}"#,
            Some(1),
        )
        .is_none());
        assert!(git_config_note("a: could not write config file x\n", Some(1)).is_none());
        // The global config is named as such, without the repository wording.
        let g = git_config_note(
            "error: could not write config file /home/u/.gitconfig: Device or resource busy\n",
            Some(1),
        )
        .expect("note");
        assert!(g.contains("/home/u/.gitconfig"), "{g}");
        assert!(!g.contains(".git/config"), "{g}");
        assert!(!g.contains("renamed"), "{g}");
        let r = git_config_note(
            "error: could not write config file .git/config: Device or resource busy\n",
            Some(1),
        )
        .expect("note");
        assert!(r.contains(".git/config") && r.contains("renamed"), "{r}");
    }

    #[test]
    fn refuses_without_sandbox_with_the_error_text() {
        let err = OxaError::new(
            oxa_proto::codes::NO_SANDBOX,
            "bwrap is not available; bash is disabled",
        )
        .with_hint("install bubblewrap");
        let sb = Fake {
            check: Some(err.clone()),
            ..Fake::ok(b"", Some(0))
        };
        let (out, _) = run_with(
            &sb,
            json!({"command": "ls"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert_eq!(out.text, err.to_string());
        assert!(
            sb.seen.lock().unwrap().is_none(),
            "nothing runs without a sandbox"
        );
    }

    /// #494: `git branch -m` renames the ref, then fails to rewrite the read-only `.git/config`
    /// ("Device or resource busy"). The result says what was done and what was not.
    #[test]
    fn git_config_write_failure_is_explained() {
        let out = b"error: could not write config file .git/config: Device or resource busy\nfatal: branch is renamed, but update of config-file failed\n";
        let sb = Fake::ok(out, Some(128));
        let (res, _) = run_with(
            &sb,
            json!({"command": "git branch -m new-name"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(res.is_error);
        assert!(res
            .text
            .contains("branch is renamed, but update of config-file failed"));
        assert!(res.text.contains("read-only in the sandbox"), "{}", res.text);
        assert!(res.text.contains("branch was renamed"), "{}", res.text);
        // An unrelated failure gets no note.
        let sb = Fake::ok(b"fatal: not a git repository\n", Some(128));
        let (res, _) = run_with(
            &sb,
            json!({"command": "git status"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(!res.text.contains("sandbox"), "{}", res.text);
    }

    #[test]
    fn git_exclude_write_failure_explains_local_directory_ignore() {
        let output = b"bash: line 1: /repo/.git/info/exclude: Read-only file system\n";
        let sb = Fake::ok(output, Some(1));
        let (res, _) = run_with(
            &sb,
            json!({"command": "printf '.venv-test/\\n' >> \"$(git rev-parse --git-path info/exclude)\""}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(res.is_error);
        assert!(res.text.contains("shared git metadata"), "{}", res.text);
        assert!(
            res.text.contains("printf '*\\n' > .venv-test/.gitignore"),
            "{}",
            res.text
        );
        assert!(
            res.text.contains("without changing tracked files"),
            "{}",
            res.text
        );

        for (out, exit) in [
            (output.as_slice(), Some(0)),
            (b"bash: /repo/log: Read-only file system\n".as_slice(), Some(1)),
            (
                b"quoted: /repo/.git/info/exclude: Permission denied\n".as_slice(),
                Some(1),
            ),
        ] {
            let sb = Fake::ok(out, exit);
            let (res, _) = run_with(
                &sb,
                json!({"command": "cat log"}),
                Mode::Workspace,
                &CancelToken::new(),
            );
            assert!(!res.text.contains("shared git metadata"), "{}", res.text);
        }
    }

    /// #253: an issue without comments prints nothing under `--comments` when piped; the result
    /// explains it. Output with real text, `--json` and other commands get no note.
    #[test]
    fn gh_comments_with_empty_output_is_explained() {
        let run = |cmd: &str, out: &[u8], exit: i32| {
            let sb = Fake::ok(out, Some(exit));
            run_with(
                &sb,
                json!({ "command": cmd }),
                Mode::Workspace,
                &CancelToken::new(),
            )
            .0
        };
        let cmd = "gh issue view 253 -R oxstack/ox-llm --comments";
        for out in [&b""[..], b"mise WARN  read-only file system\n"] {
            let res = run(cmd, out, 0);
            assert!(!res.is_error);
            // Keep tool output out of panic diagnostics; these assertions need only a static hint.
            assert!(
                res.text.contains("--json title,body,comments,state"),
                "empty gh comments should explain how to retrieve the issue text"
            );
        }
        let res = run("gh pr view 5 -c 2>&1", b"", 0);
        assert!(
            res.text.contains("only the comments"),
            "captured PR comments should get the same explanation"
        );
        for (cmd, out, exit) in [
            (cmd, &b"author:\ttester\n"[..], 0),
            (cmd, b"", 1),
            ("gh issue view 253 --json body", b"", 0),
            ("gh issue list --comments", b"", 0),
            ("true", b"", 0),
        ] {
            let res = run(cmd, out, exit);
            assert!(
                !res.text.contains("only the comments"),
                "{cmd} (exit {exit}) must not receive a comments hint"
            );
        }
    }

    /// Empty captured output does not imply no comments when stdout went elsewhere.
    #[test]
    fn gh_comments_redirected_output_gets_no_note() {
        for cmd in [
            "gh issue view 5 --comments > /tmp/comments.txt",
            "gh issue view 5 --comments >> /tmp/comments.txt",
            "gh pr view 5 -c >/tmp/comments.txt",
            "gh pr view 5 --comments 1>/tmp/comments.txt",
            "gh pr view 5 --comments 2>&1 >/tmp/comments.txt",
            "gh pr view 5 --comments | tee out >/dev/null",
            "gh pr view 5 --comments >&2",
        ] {
            let sb = Fake::ok(b"", Some(0));
            let (res, _) = run_with(
                &sb,
                json!({ "command": cmd }),
                Mode::Workspace,
                &CancelToken::new(),
            );
            assert!(!res.is_error);
            assert_eq!(res.text, "(no output)", "{cmd}");
        }
    }

    /// A successful pipeline can hide real comments, so its empty output is inconclusive.
    #[test]
    fn gh_comments_filtered_output_gets_no_note() {
        for cmd in [
            "gh issue view 5 --comments | grep -i deploy || true",
            "gh pr view 5 -c | sed -n '/x/p'",
            "gh pr view 5 --comments | grep -v author",
            "gh issue view 5 --comments|sed -n '/x/p'",
            "gh pr view 5 --comments | head -n 0",
        ] {
            let sb = Fake::ok(b"", Some(0));
            let (res, _) = run_with(
                &sb,
                json!({ "command": cmd }),
                Mode::Workspace,
                &CancelToken::new(),
            );
            assert!(!res.is_error);
            assert!(
                res.text == "(no output)",
                "a pipeline's empty output must not receive a comments hint"
            );
        }
    }

    /// #565: even verbatim git diagnostics printed by a successful inspection are just data.
    #[test]
    fn git_config_note_requires_failure_for_every_config_path() {
        for path in [".git/config", "/work/.git/config", "/home/u/.gitconfig"] {
            let output = format!("error: could not write config file {path}: Device or resource busy\n");
            for exit in [Some(0), Some(128), None] {
                let sb = Fake::ok(output.as_bytes(), exit);
                let (res, _) = run_with(
                    &sb,
                    json!({"command": "cat recorded-output"}),
                    Mode::Workspace,
                    &CancelToken::new(),
                );
                if exit == Some(0) {
                    assert!(!res.is_error);
                    assert_eq!(res.text, output, "successful inspection of {path}");
                } else {
                    assert!(res.text.contains(&format!("git could not write {path}:")));
                    assert_eq!(
                        res.text.contains("branch was renamed"),
                        path != "/home/u/.gitconfig"
                    );
                }
            }
        }
    }

    /// #565: the phrase inside quoted data of a successful command is not a git failure.
    #[test]
    fn git_config_note_ignores_quoted_text_in_successful_command() {
        let out = b"{\"body\":\"git said: could not write config file\"}\n";
        let sb = Fake::ok(out, Some(0));
        let (res, _) = run_with(
            &sb,
            json!({"command": "gh issue view 494"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(!res.is_error);
        assert!(!res.text.contains("read-only in the sandbox"), "{}", res.text);
        assert_eq!(res.text.trim_end().lines().count(), 1, "{}", res.text);
        // Even a line starting with git's words does not count when the command succeeded.
        let sb = Fake::ok(b"error: could not write config file x\n", Some(0));
        let (res, _) = run_with(
            &sb,
            json!({"command": "echo"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(!res.text.contains("read-only in the sandbox"), "{}", res.text);
    }

    #[test]
    fn exit_code_and_empty_output_lines() {
        let sb = Fake::ok(b"hi", Some(3));
        let (out, _) = run_with(&sb, json!({"command": "x"}), Mode::Workspace, &CancelToken::new());
        assert!(out.is_error);
        assert_eq!(out.text, "hi\n\nCommand exited with code 3");
        assert_eq!(out.ui, json!({"exit": 3, "timedOut": false}));

        let sb = Fake::ok(b"", Some(0));
        let (out, _) = run_with(
            &sb,
            json!({"command": "true"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(!out.is_error);
        assert_eq!(out.text, "(no output)");

        let sb = Fake::ok(b"", Some(1));
        let (out, _) = run_with(
            &sb,
            json!({"command": "false"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert_eq!(out.text, "(no output)\n\nCommand exited with code 1");
    }

    #[test]
    fn backgrounded_commands_get_the_stopped_notice() {
        assert!(looks_backgrounded("nohup ./server &"));
        assert!(looks_backgrounded("sleep 100 & "));
        assert!(looks_backgrounded("nohup make > log 2>&1"));
        assert!(!looks_backgrounded("make && make test"));
        assert!(!looks_backgrounded("echo done"));
        // Issue #127's exact multi-line example, and a line-ending `&` without nohup.
        assert!(looks_backgrounded("nohup bash x.sh >/dev/null 2>&1 &\nsleep 60"));
        assert!(looks_backgrounded("./server >/dev/null 2>&1 &\nsleep 60"));
        assert!(!looks_backgrounded("make 2>&1\necho done"));
        assert!(!looks_backgrounded("echo a &&\nmake"));

        let sb = Fake::ok(b"started", Some(0));
        let (out, _) = run_with(
            &sb,
            json!({"command": "nohup ./server &"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(
            out.text.ends_with(descriptions::errors::BASH_BACKGROUND_STOPPED),
            "{}",
            out.text
        );
        // Conditional wording: a foreground `nohup make` must not be told its processes were stopped.
        assert!(
            descriptions::errors::BASH_BACKGROUND_STOPPED.starts_with(
                "If this command started background processes (nohup or a trailing &), they were stopped when the call ended."
            ),
            "{}",
            descriptions::errors::BASH_BACKGROUND_STOPPED
        );
        let (out, _) = run_with(
            &sb,
            json!({"command": "echo hi"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(!out.text.contains("stopped"), "{}", out.text);
    }

    #[test]
    fn timeout_line_and_rounding() {
        let mut sb = Fake::ok(b"partial", None);
        sb.result.timed_out = true;
        let (out, _) = run_with(
            &sb,
            json!({"command": "sleep 9", "timeout": 1.5}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert_eq!(out.text, "partial\n\nCommand timed out after 2 seconds");
        assert_eq!(out.ui, json!({"exit": null, "timedOut": true}));
        assert_eq!(sb.seen.lock().unwrap().as_ref().unwrap().timeout_s, Some(2));
    }

    #[test]
    fn cancel_and_missing_exit_code_lines() {
        let sb = Fake::ok(b"", None);
        let cancel = CancelToken::new();
        cancel.cancel();
        let (out, _) = run_with(&sb, json!({"command": "sleep 9"}), Mode::Workspace, &cancel);
        assert!(out.is_error);
        assert_eq!(out.text, "Command aborted");

        let sb = Fake::ok(b"", None);
        let (out, _) = run_with(
            &sb,
            json!({"command": "kill -9 $$"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(out.is_error);
        assert_eq!(out.text, "(no output)\n\nCommand terminated without an exit code");
    }

    #[test]
    fn plan_mode_is_read_only_and_net_is_off() {
        let sb = Fake::ok(b"", Some(0));
        let (_, _) = run_with(&sb, json!({"command": "ls"}), Mode::Plan, &CancelToken::new());
        let seen = sb.seen.lock().unwrap().clone().expect("spec");
        assert!(!seen.writable);
        assert!(!seen.net);
        assert_eq!(seen.command, "ls");
        assert_eq!(seen.env, vec![("OXA_SESSION_ID".to_string(), "s".to_string())]);

        let sb = Fake::ok(b"", Some(0));
        let _ = run_with(
            &sb,
            json!({"command": "ls"}),
            Mode::Workspace,
            &CancelToken::new(),
        );
        assert!(sb.seen.lock().unwrap().as_ref().expect("spec").writable);
    }

    #[test]
    fn invalid_arguments_are_refused() {
        let sb = Fake::ok(b"", Some(0));
        let (out, _) = run_with(&sb, json!({"timeout": 3}), Mode::Workspace, &CancelToken::new());
        assert!(out.is_error);
        assert_eq!(out.text, MISSING_COMMAND);
        for bad in [json!(0), json!(-1), json!("soon")] {
            let (out, _) = run_with(
                &sb,
                json!({"command": "ls", "timeout": bad}),
                Mode::Workspace,
                &CancelToken::new(),
            );
            assert!(out.is_error);
            assert_eq!(out.text, errors::BASH_BAD_TIMEOUT);
        }
        assert!(sb.seen.lock().unwrap().is_none());
        let cwd = Path::new("/work");
        let cancel = CancelToken::new();
        let cx = ToolCtx {
            checked: &[],
            cwd,
            spill_dir: cwd,
            sandbox: &sb,
            mode: Mode::Workspace,
            cancel: &cancel,
            env: &[],
            tune: Default::default(),
            hidden: None,
        };
        let got = BashTool::new().access(&json!({"command": "ls", "timeout": 0}), &cx);
        assert_eq!(got, Err(errors::BASH_BAD_TIMEOUT.to_string()));
    }

    #[test]
    fn access_is_exec_without_net() {
        let sb = Fake::ok(b"", Some(0));
        let cwd = Path::new("/work");
        let cancel = CancelToken::new();
        let cx = ToolCtx {
            checked: &[],
            cwd,
            spill_dir: cwd,
            sandbox: &sb,
            mode: Mode::Ask,
            cancel: &cancel,
            env: &[],
            tune: Default::default(),
            hidden: None,
        };
        let got = BashTool::new()
            .access(&json!({"command": "cargo test"}), &cx)
            .expect("access");
        assert_eq!(
            got,
            vec![Access::Exec {
                command: "cargo test".to_string(),
                net: false
            }]
        );
    }

    #[test]
    fn streamed_chunks_split_utf8_are_joined() {
        let mut sb = Fake::ok(b"", Some(0));
        sb.chunks = vec![vec![0xE2, 0x82], vec![0xAC, b'!', 0xFF]];
        sb.result.output = "€!\u{FFFD}".as_bytes().to_vec();
        let (out, streamed) = run_with(&sb, json!({"command": "x"}), Mode::Workspace, &CancelToken::new());
        assert_eq!(streamed, "€!\u{FFFD}");
        assert_eq!(out.text, "€!\u{FFFD}");
    }

    #[test]
    fn take_valid_utf8_keeps_an_incomplete_tail() {
        let mut p = vec![b'a', 0xE2, 0x82];
        assert_eq!(take_valid_utf8(&mut p), "a");
        assert_eq!(p, vec![0xE2, 0x82]);
        p.push(0xAC);
        assert_eq!(take_valid_utf8(&mut p), "€");
        assert!(p.is_empty());
    }

    fn run_tuned(sb: &Fake, args: Value, tune: oxa_proto::ToolTune) -> ToolOutput {
        let cwd = Path::new("/work");
        let env: Vec<(String, String)> = Vec::new();
        let cancel = CancelToken::new();
        let cx = ToolCtx {
            checked: &[],
            cwd,
            spill_dir: cwd,
            sandbox: sb,
            mode: Mode::Workspace,
            cancel: &cancel,
            env: &env,
            tune,
            hidden: None,
        };
        BashTool::new().run(&args, &cx, &mut |_| {})
    }

    /// F5: `bash_timeout_s` applies only when the call names no timeout; the timeout text is
    /// the existing one.
    #[test]
    fn tune_timeout_is_the_default_and_an_explicit_timeout_wins() {
        let tuned = oxa_proto::ToolTune {
            bash_timeout_s: 7,
            ..Default::default()
        };
        let sb = Fake::ok(b"", Some(0));
        let _ = run_tuned(&sb, json!({"command": "sleep 9"}), tuned);
        assert_eq!(sb.seen.lock().unwrap().as_ref().unwrap().timeout_s, Some(7));

        let sb = Fake::ok(b"", Some(0));
        let _ = run_tuned(&sb, json!({"command": "sleep 9", "timeout": 2}), tuned);
        assert_eq!(sb.seen.lock().unwrap().as_ref().unwrap().timeout_s, Some(2));

        let sb = Fake::ok(b"", Some(0));
        let _ = run_tuned(&sb, json!({"command": "sleep 9"}), Default::default());
        assert_eq!(sb.seen.lock().unwrap().as_ref().unwrap().timeout_s, None);

        let mut sb = Fake::ok(b"partial", None);
        sb.result.timed_out = true;
        let out = run_tuned(&sb, json!({"command": "sleep 9"}), tuned);
        assert!(out.is_error);
        assert_eq!(out.text, "partial\n\nCommand timed out after 7 seconds");
    }

    /// Diet: the default keeps the bytes; the diet cuts runs of three or more blank lines to two
    /// and changes nothing else.
    #[test]
    fn diet_cuts_long_blank_runs_only() {
        let raw = "Compiling a v1\n   \n\t\n\nerror: boom  \n\n\n\n\nnext\t\t\n";
        let expected = "Compiling a v1\n   \n\t\nerror: boom  \n\n\nnext\t\t\n";
        let sb = Fake::ok(raw.as_bytes(), Some(0));
        let plain = run_tuned(&sb, json!({"command": "x"}), Default::default());
        assert_eq!(plain.text, raw);

        let sb = Fake::ok(raw.as_bytes(), Some(0));
        let diet = oxa_proto::ToolTune {
            diet: true,
            ..Default::default()
        };
        let out = run_tuned(&sb, json!({"command": "x"}), diet);
        assert_eq!(out.text, expected);
        assert!(!out.is_error);

        // Deterministic and idempotent; one or two blank lines and trailing blanks are kept.
        assert_eq!(diet_output(expected), expected);
        assert_eq!(diet_output(""), "");
        assert_eq!(diet_output("\n"), "\n");
        assert_eq!(diet_output("a\n\n\nb"), "a\n\n\nb");
        assert_eq!(diet_output("a \n\n\n\n\n b\t"), "a \n\n\n b\t");
    }

    /// The exit-status line follows the shaped output unchanged.
    #[test]
    fn diet_keeps_the_status_lines() {
        let diet = oxa_proto::ToolTune {
            diet: true,
            ..Default::default()
        };
        let sb = Fake::ok(b"x  \n\n\n\n\n", Some(3));
        let out = run_tuned(&sb, json!({"command": "x"}), diet);
        assert!(out.is_error);
        assert!(out.text.starts_with("x  \n"), "{:?}", out.text);
        assert!(out.text.ends_with("Command exited with code 3"), "{:?}", out.text);
        assert_eq!(out.ui, json!({"exit": 3, "timedOut": false}));
    }

    // ---------------------------------------------------------------------------------
    // Issue #127 (b): background jobs.

    /// A sandbox that blocks until its cancel token is set, then reports "killed".
    struct Blocking {
        seen: Mutex<Option<ExecSpec>>,
    }

    impl Sandbox for Blocking {
        fn check(&self) -> Result<(), OxaError> {
            Ok(())
        }
        fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
            *self.seen.lock().unwrap() = Some(s.clone());
            live(b"started\n");
            while !c.is_cancelled() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            ExecResult {
                output: b"started\n".to_vec(),
                exit: None,
                timed_out: false,
                unsandboxed: false,
            }
        }
    }

    /// Build a `BashTool` sharing `jobs` with a sandbox, and run one call.
    fn run_job_tool(tool: &BashTool, sb: &dyn Sandbox, args: Value) -> ToolOutput {
        let cwd = Path::new("/work");
        let env: Vec<(String, String)> = Vec::new();
        let cancel = CancelToken::new();
        let cx = ToolCtx {
            checked: &[],
            cwd,
            spill_dir: cwd,
            sandbox: sb,
            mode: Mode::Workspace,
            cancel: &cancel,
            env: &env,
            tune: Default::default(),
            hidden: None,
        };
        tool.run(&args, &cx, &mut |_| {})
    }

    #[test]
    fn background_start_returns_a_job_id() {
        let jobs = Arc::new(JobRegistry::new());
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        jobs.set_sandbox(sb.clone());
        let tool = BashTool::with_shell_and_jobs(ShellConfig::default(), jobs.clone());
        let out = run_job_tool(&tool, &*sb, json!({"command": "sleep 100", "background": true}));
        assert!(!out.is_error, "{:?}", out.text);
        assert_eq!(out.ui, json!({"job": "job-1"}));
        assert!(
            out.text.contains("Started background job job-1"),
            "{:?}",
            out.text
        );
        jobs.kill_all();
    }

    /// #618: process isolation is intentional, but must not make a running job look lost.
    #[test]
    fn background_job_responses_explain_process_visibility() {
        let jobs = Arc::new(JobRegistry::new());
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        jobs.set_sandbox(sb.clone());
        let tool = BashTool::with_shell_and_jobs(ShellConfig::default(), jobs.clone());
        let started = run_job_tool(&tool, &*sb, json!({"command": "sleep 100", "background": true}));
        let running = run_job_tool(&tool, &*sb, json!({"job": "job-1"}));
        jobs.kill_all();

        assert!(!started.is_error, "{}", started.text);
        assert!(started.text.contains(JOB_PROCESS_VISIBILITY), "{}", started.text);
        assert!(
            started.text.contains("bash {\"job\": \"job-1\"}"),
            "{}",
            started.text
        );
        assert!(!running.is_error, "{}", running.text);
        assert!(running.text.contains("Still running"), "{}", running.text);
        assert!(
            running.text.contains("bash {\"job\": \"job-1\", \"wait_s\": 30}"),
            "{}",
            running.text
        );
        assert!(!running.text.contains("separate PID namespace"));
        assert!(!running.text.contains("ps in another bash call"));
        assert!(!running.text.contains("Use the job interface"));
    }

    #[test]
    fn job_read_returns_status_and_output() {
        let jobs = Arc::new(JobRegistry::new());
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        jobs.set_sandbox(sb.clone());
        let tool = BashTool::with_shell_and_jobs(ShellConfig::default(), jobs.clone());
        let _ = run_job_tool(&tool, &*sb, json!({"command": "sleep 100", "background": true}));
        std::thread::sleep(std::time::Duration::from_millis(50));
        let out = run_job_tool(&tool, &*sb, json!({"job": "job-1"}));
        assert!(!out.is_error, "{:?}", out.text);
        assert_eq!(out.ui, json!({"job": "job-1", "status": "running"}));
        assert!(out.text.starts_with("Job job-1: running"), "{:?}", out.text);
        assert!(out.text.contains("started"), "{:?}", out.text);
        jobs.kill_all();
    }

    #[test]
    fn job_read_unknown_is_an_error() {
        let jobs = Arc::new(JobRegistry::new());
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        jobs.set_sandbox(sb.clone());
        let tool = BashTool::with_shell_and_jobs(ShellConfig::default(), jobs.clone());
        let out = run_job_tool(&tool, &*sb, json!({"job": "job-99"}));
        assert!(out.is_error);
        assert_eq!(out.text, "Unknown job: job-99");
    }

    #[test]
    fn background_with_job_is_refused() {
        let jobs = Arc::new(JobRegistry::new());
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        jobs.set_sandbox(sb.clone());
        let tool = BashTool::with_shell_and_jobs(ShellConfig::default(), jobs.clone());
        // `background` with a `job` (and no `command`) is refused.
        let out = run_job_tool(&tool, &*sb, json!({"job": "job-1", "background": true}));
        assert!(out.is_error);
        assert_eq!(out.text, BACKGROUND_NEEDS_COMMAND);
        // Both `command` and `job` is refused as "exactly one".
        let out = run_job_tool(&tool, &*sb, json!({"command": "x", "job": "job-1"}));
        assert!(out.is_error);
        assert_eq!(out.text, COMMAND_OR_JOB);
    }

    #[test]
    fn neither_command_nor_job_is_missing_command() {
        let jobs = Arc::new(JobRegistry::new());
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        jobs.set_sandbox(sb.clone());
        let tool = BashTool::with_shell_and_jobs(ShellConfig::default(), jobs.clone());
        let out = run_job_tool(&tool, &*sb, json!({}));
        assert!(out.is_error);
        assert_eq!(out.text, MISSING_COMMAND);
    }

    #[test]
    fn fifth_job_is_refused() {
        let jobs = Arc::new(JobRegistry::new());
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        jobs.set_sandbox(sb.clone());
        let tool = BashTool::with_shell_and_jobs(ShellConfig::default(), jobs.clone());
        for _ in 0..4 {
            let out = run_job_tool(&tool, &*sb, json!({"command": "sleep 100", "background": true}));
            assert!(!out.is_error, "{:?}", out.text);
        }
        let out = run_job_tool(&tool, &*sb, json!({"command": "sleep 100", "background": true}));
        assert!(out.is_error);
        assert!(out.text.contains("At most 4 background jobs"), "{:?}", out.text);
        jobs.kill_all();
    }

    /// #519: `bash {job, kill: true}` stops one job and frees its slot; the others keep running.
    #[test]
    fn kill_stops_one_job_and_frees_its_slot() {
        let jobs = Arc::new(JobRegistry::new());
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        jobs.set_sandbox(sb.clone());
        let tool = BashTool::with_shell_and_jobs(ShellConfig::default(), jobs.clone());
        for _ in 0..4 {
            let out = run_job_tool(&tool, &*sb, json!({"command": "sleep 100", "background": true}));
            assert!(!out.is_error, "{:?}", out.text);
        }
        let full = run_job_tool(&tool, &*sb, json!({"command": "sleep 100", "background": true}));
        assert!(full.is_error);
        let out = run_job_tool(&tool, &*sb, json!({"job": "job-2", "kill": true}));
        assert!(!out.is_error, "{:?}", out.text);
        assert_eq!(out.ui, json!({"job": "job-2", "status": "killed"}));
        assert!(out.text.starts_with("Job job-2: killed"), "{:?}", out.text);
        assert_eq!(jobs.running(), 3);
        let again = run_job_tool(&tool, &*sb, json!({"command": "sleep 100", "background": true}));
        assert!(!again.is_error, "{:?}", again.text);
        // Killing a finished job is not an error and says so.
        let done = run_job_tool(&tool, &*sb, json!({"job": "job-2", "kill": true}));
        assert!(!done.is_error, "{:?}", done.text);
        assert!(done.text.contains("already finished"), "{:?}", done.text);
        let unknown = run_job_tool(&tool, &*sb, json!({"job": "job-99", "kill": true}));
        assert_eq!(unknown.text, "Unknown job: job-99");
        jobs.kill_all();
    }

    /// #519: `kill` only makes sense with `job`.
    #[test]
    fn kill_needs_a_job() {
        let jobs = Arc::new(JobRegistry::new());
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        jobs.set_sandbox(sb.clone());
        let tool = BashTool::with_shell_and_jobs(ShellConfig::default(), jobs.clone());
        let out = run_job_tool(&tool, &*sb, json!({"command": "ls", "kill": true}));
        assert!(out.is_error);
        assert!(out.text.contains("\"kill\" applies to a job"), "{:?}", out.text);
    }

    /// #519: a `timeout` given with `background: true` is the job's lifetime cap; without it
    /// there is still none.
    #[test]
    fn background_job_honours_an_explicit_timeout() {
        let jobs = Arc::new(JobRegistry::new());
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        jobs.set_sandbox(sb.clone());
        let tool = BashTool::with_shell_and_jobs(ShellConfig::default(), jobs.clone());
        let _ = run_job_tool(
            &tool,
            &*sb,
            json!({"command": "sleep 100", "background": true, "timeout": 1.5}),
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(sb.seen.lock().unwrap().as_ref().unwrap().timeout_s, Some(2));
        jobs.kill_all();
    }

    #[test]
    fn session_end_kills_jobs() {
        let jobs = Arc::new(JobRegistry::new());
        let sb = Arc::new(Blocking {
            seen: Mutex::new(None),
        });
        jobs.set_sandbox(sb.clone());
        let tool = BashTool::with_shell_and_jobs(ShellConfig::default(), jobs.clone());
        let _ = run_job_tool(&tool, &*sb, json!({"command": "sleep 100", "background": true}));
        std::thread::sleep(std::time::Duration::from_millis(50));
        jobs.kill_all();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let out = run_job_tool(&tool, &*sb, json!({"job": "job-1"}));
            if out.ui == json!({"job": "job-1", "status": "killed"}) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "job did not settle as killed: {:?}",
                out.text
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
mod poll_tests {
    use super::*;
    use oxa_proto::{CancelToken, ExecResult, Mode, OxaError, Sandbox};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::Mutex;

    /// A job whose output is sent by the test, one chunk at a time, and that ends when the
    /// sender is dropped (exit 0) or the job is cancelled.
    struct Chunks {
        rx: Mutex<Receiver<Vec<u8>>>,
    }

    impl Sandbox for Chunks {
        fn check(&self) -> Result<(), OxaError> {
            Ok(())
        }
        fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, _s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
            let rx = self.rx.lock().unwrap();
            loop {
                if c.is_cancelled() {
                    return ExecResult {
                        output: Vec::new(),
                        exit: None,
                        timed_out: false,
                        unsandboxed: false,
                    };
                }
                match rx.recv_timeout(std::time::Duration::from_millis(5)) {
                    Ok(chunk) => live(&chunk),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => {
                        return ExecResult {
                            output: Vec::new(),
                            exit: Some(0),
                            timed_out: false,
                            unsandboxed: false,
                        }
                    }
                }
            }
        }
    }

    fn call(tool: &BashTool, sb: &dyn Sandbox, args: Value) -> ToolOutput {
        let cwd = std::path::Path::new("/work");
        let env: Vec<(String, String)> = Vec::new();
        let cancel = CancelToken::new();
        let cx = ToolCtx {
            checked: &[],
            cwd,
            spill_dir: cwd,
            sandbox: sb,
            mode: Mode::Workspace,
            cancel: &cancel,
            env: &env,
            tune: Default::default(),
            hidden: None,
        };
        tool.run(&args, &cx, &mut |_| {})
    }

    fn setup() -> (BashTool, Arc<JobRegistry>, Arc<Chunks>, Sender<Vec<u8>>) {
        let (tx, rx) = channel();
        let jobs = Arc::new(JobRegistry::new());
        let sb = Arc::new(Chunks { rx: Mutex::new(rx) });
        jobs.set_sandbox(sb.clone());
        let tool = BashTool::with_shell_and_jobs(ShellConfig::default(), jobs.clone());
        let started = call(&tool, &*sb, json!({"command": "tail -f log", "background": true}));
        assert!(!started.is_error, "{}", started.text);
        (tool, jobs, sb, tx)
    }

    /// #150 (1): a poll whose output grew is a different result even when the last
    /// [`READ_CAP`] bytes read the same (a job printing a long run of one character). The
    /// repeat guard compares result text, so the text must say how much output arrived.
    #[test]
    fn a_poll_reports_how_much_output_arrived() {
        let (tool, jobs, sb, tx) = setup();
        let n = crate::jobs::READ_CAP + 800;
        tx.send(vec![b'x'; n]).unwrap();
        while jobs.buffered_len("job-1") != Some(n) {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let first = call(&tool, &*sb, json!({"job": "job-1"})).text;
        tx.send(vec![b'x'; 800]).unwrap();
        while jobs.buffered_len("job-1") != Some(n + 800) {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let second = call(&tool, &*sb, json!({"job": "job-1"})).text;
        assert_ne!(first, second);
        jobs.kill_all();
    }

    /// #150 (2): `wait_s` returns as soon as the job prints new output, well before its cap.
    #[test]
    fn wait_returns_early_on_new_output() {
        let (tool, jobs, sb, tx) = setup();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            tx.send(b"hello\n".to_vec()).unwrap();
            tx
        });
        let started = std::time::Instant::now();
        let out = call(&tool, &*sb, json!({"job": "job-1", "wait_s": 20}));
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(10), "{took:?}");
        assert!(out.text.contains("hello\n"), "{:?}", out.text);
        assert!(out.text.contains("Still running"), "{:?}", out.text);
        drop(sender.join().unwrap());
        jobs.kill_all();
    }

    /// #150 (2): `wait_s` returns as soon as the job exits, and then says so (no retry hint).
    #[test]
    fn wait_returns_early_on_exit() {
        let (tool, _jobs, sb, tx) = setup();
        let sender = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(tx);
        });
        let started = std::time::Instant::now();
        let out = call(&tool, &*sb, json!({"job": "job-1", "wait_s": 20}));
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(out.text.starts_with("Job job-1: exited 0"), "{:?}", out.text);
        assert!(!out.text.contains("Still running"), "{:?}", out.text);
        sender.join().unwrap();
    }

    /// #150 (2): with no news the read waits about `wait_s`, then tells the model to wait or
    /// poll again later.
    #[test]
    fn wait_without_news_runs_out_and_says_to_wait() {
        let (tool, jobs, sb, _tx) = setup();
        let started = std::time::Instant::now();
        let out = call(&tool, &*sb, json!({"job": "job-1", "wait_s": 0.3}));
        let took = started.elapsed();
        assert!(took >= std::time::Duration::from_millis(280), "{took:?}");
        assert!(out.text.contains("wait again later") || out.text.contains("poll again later"));
        assert!(out.text.contains("do not retry at once"), "{:?}", out.text);
        jobs.kill_all();
    }

    /// #150 (2): the cap, zero/absent, and the refused values.
    #[test]
    fn wait_cap_and_bad_values() {
        let dur = |v: Value| parse_wait(&v);
        assert_eq!(dur(json!({})), Ok(None));
        assert_eq!(dur(json!({"wait_s": null})), Ok(None));
        assert_eq!(dur(json!({"wait_s": 0})), Ok(None));
        assert_eq!(
            dur(json!({"wait_s": 5})),
            Ok(Some(std::time::Duration::from_secs(5)))
        );
        assert_eq!(
            dur(json!({"wait_s": 1000})),
            Ok(Some(std::time::Duration::from_secs(60)))
        );
        assert_eq!(
            dur(json!({"wait_s": WAIT_CAP_S})),
            Ok(Some(std::time::Duration::from_secs(60)))
        );
        for bad in [json!(-1), json!("5"), json!(true), json!([1])] {
            assert_eq!(
                dur(json!({ "wait_s": bad.clone() })),
                Err(BAD_WAIT.to_string()),
                "{bad}"
            );
        }
        // Only a job read may wait.
        assert_eq!(
            parse(&json!({"command": "ls", "wait_s": 3})),
            Err(WAIT_NEEDS_JOB.to_string())
        );
        assert!(parse(&json!({"command": "ls", "wait_s": null})).is_ok());
        // Through the tool: a bad value is an error result, not a wait.
        let (tool, jobs, sb, _tx) = setup();
        let out = call(&tool, &*sb, json!({"job": "job-1", "wait_s": -2}));
        assert!(out.is_error && out.text == BAD_WAIT, "{:?}", out.text);
        jobs.kill_all();
    }

    #[test]
    fn schema_documents_wait_s() {
        let tool = BashTool::new();
        let schema = tool.spec().schema_json.clone();
        assert!(schema.contains("wait_s") && schema.contains("max 60"), "{schema}");
    }
}
