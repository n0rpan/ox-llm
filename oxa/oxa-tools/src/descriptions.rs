// ported from pi packages/coding-agent/src/core/tools/{read,write,edit,bash,grep,find,ls,truncate,edit-diff}.ts
//! W0-FROZEN (architect-owned): every text a model sees about a built-in tool, and every
//! fixed text a built-in tool returns. Tasks A5-A9 use these constants; they never write a
//! model-facing literal of their own. A wording change is a prompt change: architect only.
//!
//! Source: pi 1.5.2 @47e9d74. Texts are pi's, verbatim, except where marked `ADAPTED`:
//! * pi's byte limit (50KB) is oxa's W4 cap (16KB default, `oxa.toolOutputCap`); the
//!   descriptions name the default, as pi's descriptions name pi's default.
//! * `read`: pi's sentence about images being sent as attachments is dropped (the engine is
//!   text-only; an image yields `oxa_proto::image_placeholder`).
//! * `bash`: pi keeps the *last* lines; oxa keeps head + tail and names a spill file (W4).
//! * `PI_*` -> `OXA_*`.
//!
//! Schemas are the JSON Schema that pi's TypeBox definitions serialise to, as canonical text
//! with pi's property order. Key order inside an object schema is `type, properties, required`
//! (oxa's canonical choice; TypeBox's own order is not load-bearing for a local template).

use oxa_proto::ToolClass;

/// The model-facing text of one built-in tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolText {
    pub name: &'static str,
    pub description: &'static str,
    /// Canonical JSON text of the parameter schema (part of the frozen prefix bytes, W3).
    pub schema_json: &'static str,
    pub class: ToolClass,
    /// pi's `promptSnippet`: the one-line entry in the `<tools>` prompt section.
    pub prompt_snippet: &'static str,
    /// pi's `promptGuidelines`: bullets added to the `<rules>` prompt section when selected.
    pub guidelines: &'static [&'static str],
}

/// pi's limits, and oxa's cap.
pub mod limits {
    /// `read`: default page size in lines (pi `DEFAULT_MAX_LINES`).
    pub const READ_MAX_LINES: usize = 2000;
    /// pi's byte limit (`DEFAULT_MAX_BYTES`, 50 KB). Kept for PARITY notes and `/import`;
    /// oxa output is capped by [`TOOL_OUTPUT_CAP_DEFAULT`] instead.
    pub const PI_MAX_BYTES: usize = 50 * 1024;
    /// W4: default cap of any tool output on the wire (`oxa.toolOutputCap`).
    pub const TOOL_OUTPUT_CAP_DEFAULT: usize = 16 * 1024;
    /// `grep`: default match limit.
    pub const GREP_LIMIT: usize = 100;
    /// `grep`: longest match line in characters before it is cut (pi `GREP_MAX_LINE_LENGTH`).
    pub const GREP_MAX_LINE_LENGTH: usize = 500;
    /// `find`: default result limit.
    pub const FIND_LIMIT: usize = 1000;
    /// `ls`: default entry limit.
    pub const LS_LIMIT: usize = 500;
    /// A `limit` below 1 (or fractional) is `max(1, floor(limit))` in grep, find and ls.
    pub const MIN_LIMIT: usize = 1;
}

/// ADAPTED: pi says "2000 lines or 50KB" and mentions image attachments.
pub const READ: ToolText = ToolText {
    name: "read",
    description: "Read the contents of a file. For text files, output is truncated to 2000 lines or 16KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.",
    schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Path to the file to read (relative or absolute)"},"offset":{"type":"number","description":"Line number to start reading from (1-indexed)"},"limit":{"type":"number","description":"Maximum number of lines to read"}},"required":["path"]}"#,
    class: ToolClass::Read,
    prompt_snippet: "Read file contents",
    guidelines: &["Use read to examine files instead of cat or sed."],
};

/// ADAPTED: pi says "truncated to last 2000 lines or 50KB ... saved to a temp file".
pub const BASH: ToolText = ToolText {
    name: "bash",
    description: "Execute a bash command in the current working directory. Returns stdout and stderr. Output over 16KB is cut to its first and last part. If truncated, full output is saved to a file. Optionally provide a timeout in seconds.",
    schema_json: r#"{"type":"object","properties":{"command":{"type":"string","description":"Shell command to execute"},"timeout":{"type":"number","description":"Timeout in seconds (optional, no default timeout)"}},"required":["command"]}"#,
    class: ToolClass::Exec,
    prompt_snippet: "Execute bash commands (ls, grep, find, etc.)",
    guidelines: &[
        "You can inspect OXA_* environment variables for current model and session details.",
        "Bash sandbox persistence: workspace writes persist except protected paths; configured writable binds, package caches and permitted git data also persist. Other paths are read-only or hidden. /tmp is private scratch discarded after each command, even on success. Save deliverables in the working directory, never claim a scratch file was saved on the host, and report write failures instead of claiming success. Plan mode keeps workspace files read-only.",
    ],
};

pub const EDIT: ToolText = ToolText {
    name: "edit",
    description: "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.",
    schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Path to the file to edit (relative or absolute)"},"edits":{"type":"array","description":"One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.","items":{"type":"object","properties":{"oldText":{"type":"string","description":"Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},"newText":{"type":"string","description":"Replacement text for this targeted edit."}},"required":["oldText","newText"]}}},"required":["path","edits"]}"#,
    class: ToolClass::Write,
    prompt_snippet: "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
    guidelines: &[
        "Use edit for targeted changes. Batch changes to one file in a single call.",
        "Each edits[].oldText must exactly and uniquely match the original file. Keep matches short and non-overlapping; merge nearby changes without including large unchanged regions.",
    ],
};

/// ADAPTED (profile `local` only, OXA-LOCAL-FIRST T1): pi's `edit` declared with three string
/// parameters instead of the `edits[]` array, so a model that writes calls as
/// `<parameter=..>` blocks writes code as raw text, not as escaped JSON. Derived from [`EDIT`];
/// only the parameter part of the texts changes. Several changes to one file = several calls
/// in one reply, run in order (same-path writes are serialised), each matched against the file
/// as the previous call left it. The tool accepts both forms under both profiles.
pub const EDIT_FLAT: ToolText = ToolText {
    name: "edit",
    description: "Edit a single file using exact text replacement. oldText must match a unique region of the file. For several changes to one file, make one edit call per change in the same reply; the calls run in order, each on the file as the previous call left it. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.",
    schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Path to the file to edit (relative or absolute)"},"oldText":{"type":"string","description":"Exact text for one targeted replacement. It must be unique in the file."},"newText":{"type":"string","description":"Replacement text for this targeted edit."}},"required":["path","oldText","newText"]}"#,
    class: ToolClass::Write,
    prompt_snippet: "Make precise file edits with exact text replacement, one call per change",
    guidelines: &[
        "Use edit for targeted changes. For several changes to one file, make several edit calls in one reply.",
        "Each oldText must exactly and uniquely match the file as the previous edit left it. Keep matches short; merge nearby changes without including large unchanged regions.",
    ],
};

pub const WRITE: ToolText = ToolText {
    name: "write",
    description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
    schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Path to the file to write (relative or absolute)"},"content":{"type":"string","description":"Content to write to the file"}},"required":["path","content"]}"#,
    class: ToolClass::Write,
    prompt_snippet: "Create or overwrite files",
    guidelines: &["Use write only for new files or complete rewrites."],
};

/// ADAPTED: pi says "100 matches or 50KB".
pub const GREP: ToolText = ToolText {
    name: "grep",
    description: "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 16KB (whichever is hit first). Long lines are truncated to 500 chars.",
    schema_json: r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Search pattern (regex or literal string)"},"path":{"type":"string","description":"Directory or file to search (default: current directory)"},"glob":{"type":"string","description":"Filter files by glob pattern relative to path, e.g. 'src/**/*.ts'. Without '/', e.g. '*.ts', it matches file names at any depth"},"ignoreCase":{"type":"boolean","description":"Case-insensitive search (default: false)"},"literal":{"type":"boolean","description":"Treat pattern as literal string instead of regex (default: false)"},"context":{"type":"number","description":"Number of lines to show before and after each match (default: 0)"},"limit":{"type":"number","description":"Maximum number of matches to return (default: 100)"}},"required":["pattern"]}"#,
    class: ToolClass::Read,
    prompt_snippet: "Search file contents for patterns (respects .gitignore)",
    guidelines: &[],
};

/// ADAPTED: pi says "1000 results or 50KB".
pub const FIND: ToolText = ToolText {
    name: "find",
    description: "Search for files by glob pattern. Returns matching file paths relative to the search directory. Respects .gitignore. Output is truncated to 1000 results or 16KB (whichever is hit first).",
    schema_json: r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Glob pattern relative to the search directory, e.g. 'src/**/*.ts'. Without '/', e.g. '*.ts', it matches file names at any depth"},"path":{"type":"string","description":"Directory to search in (default: current directory)"},"limit":{"type":"number","description":"Maximum number of results (default: 1000)"}},"required":["pattern"]}"#,
    class: ToolClass::Read,
    prompt_snippet: "Find files by glob pattern (respects .gitignore)",
    guidelines: &[],
};

/// ADAPTED: pi says "500 entries or 50KB".
pub const LS: ToolText = ToolText {
    name: "ls",
    description: "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 16KB (whichever is hit first).",
    schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Directory to list (default: current directory)"},"limit":{"type":"number","description":"Maximum number of entries to return (default: 500)"}}}"#,
    class: ToolClass::Read,
    prompt_snippet: "List directory contents",
    guidelines: &[],
};

/// All seven built-ins in pi's order (the four pi defaults first). oxa enables all seven by
/// default; the frozen tool list is sorted by name by `build_prefix`, not by this order.
pub const ALL: [&ToolText; 7] = [&READ, &BASH, &EDIT, &WRITE, &GREP, &FIND, &LS];

/// Look a built-in up by name.
pub fn by_name(name: &str) -> Option<&'static ToolText> {
    ALL.into_iter().find(|t| t.name == name)
}

/// pi's exact result and error texts. `path` is always the path **as the model wrote it**
/// (not the resolved absolute path), as in pi.
pub mod errors {
    // ---- shared ----
    pub const ABORTED: &str = "Operation aborted";
    pub fn path_not_found(path: &str) -> String {
        format!("Path not found: {path}")
    }
    /// pi's `formatSize`: `{n}B`, `{n.n}KB`, `{n.n}MB`.
    pub fn format_size(bytes: usize) -> String {
        if bytes < 1024 {
            format!("{bytes}B")
        } else if bytes < 1024 * 1024 {
            format!("{:.1}KB", bytes as f64 / 1024.0)
        } else {
            format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
        }
    }

    // ---- read ----
    pub fn read_offset_beyond_end(offset: usize, total_lines: usize) -> String {
        format!("Offset {offset} is beyond end of file ({total_lines} lines total)")
    }
    /// Notice appended after `\n\n` when the line limit cut the page.
    pub fn read_more_by_lines(start: usize, end: usize, total: usize, next_offset: usize) -> String {
        format!("[Showing lines {start}-{end} of {total}. Use offset={next_offset} to continue.]")
    }
    /// Notice appended after `\n\n` when the byte cap cut the page (`cap` = the active cap).
    pub fn read_more_by_bytes(
        start: usize,
        end: usize,
        total: usize,
        cap: usize,
        next_offset: usize,
    ) -> String {
        format!(
            "[Showing lines {start}-{end} of {total} ({} limit). Use offset={next_offset} to continue.]",
            format_size(cap)
        )
    }
    /// Notice appended after `\n\n` when the caller's `limit` stopped before the end.
    pub fn read_more_after_limit(remaining: usize, next_offset: usize) -> String {
        format!("[{remaining} more lines in file. Use offset={next_offset} to continue.]")
    }
    /// The whole output when the first requested line alone exceeds the cap.
    pub fn read_line_too_long(line: usize, line_size: usize, cap: usize, path: &str) -> String {
        format!(
            "[Line {line} is {}, exceeds {} limit. Use bash: sed -n '{line}p' {path} | head -c {cap}]",
            format_size(line_size),
            format_size(cap)
        )
    }
    /// oxa addition (pi has no such refusal): a file with a NUL byte in its first 8 KiB that
    /// is not a supported image.
    pub fn read_binary_refused(path: &str) -> String {
        format!("Cannot read binary file: {path}")
    }

    // ---- write ----
    pub fn write_ok(path: &str) -> String {
        format!("Successfully wrote to {path}")
    }

    // ---- edit (edit.ts, edit-diff.ts) ----
    pub const EDIT_NO_EDITS: &str =
        "Edit tool input is invalid. edits must contain at least one replacement.";
    pub fn edit_ok(count: usize, path: &str) -> String {
        format!("Successfully replaced {count} block(s) in {path}.")
    }
    /// Wrapper for an I/O failure; `cause` has no trailing period.
    pub fn edit_could_not(path: &str, cause: &str) -> String {
        format!("Could not edit file: {path}. {cause}.")
    }
    pub fn edit_not_found(path: &str, index: usize, total: usize) -> String {
        if total == 1 {
            format!("Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines.")
        } else {
            format!("Could not find edits[{index}] in {path}. The oldText must match exactly including all whitespace and newlines.")
        }
    }
    pub fn edit_duplicate(path: &str, index: usize, total: usize, occurrences: usize) -> String {
        if total == 1 {
            format!("Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique.")
        } else {
            format!("Found {occurrences} occurrences of edits[{index}] in {path}. Each oldText must be unique. Please provide more context to make it unique.")
        }
    }
    pub fn edit_empty_old_text(path: &str, index: usize, total: usize) -> String {
        if total == 1 {
            format!("oldText must not be empty in {path}.")
        } else {
            format!("edits[{index}].oldText must not be empty in {path}.")
        }
    }
    pub fn edit_no_change(path: &str, total: usize) -> String {
        if total == 1 {
            format!("No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected.")
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        }
    }
    /// `a` < `b` are the indices of the two edits, in match order.
    pub fn edit_overlap(a: usize, b: usize, path: &str) -> String {
        format!("edits[{a}] and edits[{b}] overlap in {path}. Merge them into one edit or target disjoint regions.")
    }

    // ---- bash ----
    pub const BASH_NO_OUTPUT: &str = "(no output)";
    pub const BASH_ABORTED: &str = "Command aborted";
    pub const BASH_NO_EXIT_CODE: &str = "Command terminated without an exit code";
    pub const BASH_BAD_TIMEOUT: &str = "Invalid timeout: must be a finite number of seconds";
    pub fn bash_exit(code: i32) -> String {
        format!("Command exited with code {code}")
    }
    pub fn bash_timed_out(seconds: u32) -> String {
        format!("Command timed out after {seconds} seconds")
    }
    /// Appended to [`bash_timed_out`] when the shell itself had exited but background
    /// processes kept the output open (pi #11613). oxa omits pi's "after N seconds" (W4: no
    /// timings on the wire).  ADAPTED.
    pub fn bash_timed_out_background(shell_exit: i32) -> String {
        format!(". The shell exited with code {shell_exit}, but background processes it started kept writing output until the timeout.")
    }
    pub fn bash_cwd_missing(cwd: &str) -> String {
        format!("Working directory does not exist: {cwd}\nCannot execute bash commands.")
    }
    /// pi's `appendStatus`: output, a blank line, then the status; the status alone when the
    /// output is empty.
    pub fn bash_with_status(output: &str, status: &str) -> String {
        if output.is_empty() {
            status.to_string()
        } else {
            format!("{output}\n\n{status}")
        }
    }

    // ---- grep / find / ls ----
    pub const GREP_NO_MATCHES: &str = "No matches found";
    pub const FIND_NO_FILES: &str = "No files found matching pattern";
    pub const LS_EMPTY: &str = "(empty directory)";
    pub fn ls_not_a_directory(path: &str) -> String {
        format!("Not a directory: {path}")
    }
    pub fn ls_cannot_read(cause: &str) -> String {
        format!("Cannot read directory: {cause}")
    }
    pub fn grep_limit_notice(limit: usize) -> String {
        format!(
            "{limit} matches limit reached. Use limit={} for more, or refine pattern",
            limit * 2
        )
    }
    pub fn grep_lines_truncated_notice() -> String {
        format!(
            "Some lines truncated to {} chars. Use read tool to see full lines",
            super::limits::GREP_MAX_LINE_LENGTH
        )
    }
    pub fn find_limit_notice(limit: usize) -> String {
        format!(
            "{limit} results limit reached. Use limit={} for more, or refine pattern",
            limit * 2
        )
    }
    pub fn ls_limit_notice(limit: usize) -> String {
        format!("{limit} entries limit reached. Use limit={} for more", limit * 2)
    }
    pub fn byte_limit_notice(cap: usize) -> String {
        format!("{} limit reached", format_size(cap))
    }
    /// How grep / find / ls append their notices: `\n\n[` + notices joined by `. ` + `]`.
    /// Empty when there are none.
    pub fn notices_suffix(notices: &[String]) -> String {
        if notices.is_empty() {
            String::new()
        } else {
            format!("\n\n[{}]", notices.join(". "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_schemas_are_objects() {
        let mut names: Vec<&str> = ALL.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 7);
        for t in ALL {
            assert!(
                t.schema_json.starts_with(r#"{"type":"object","properties":{"#),
                "{}",
                t.name
            );
            assert!(
                !t.description.contains("50KB") && !t.description.contains("PI_"),
                "{}",
                t.name
            );
            assert_eq!(by_name(t.name).map(|x| x.name), Some(t.name));
        }
    }

    #[test]
    fn pi_texts() {
        assert_eq!(errors::format_size(16 * 1024), "16.0KB");
        assert_eq!(errors::format_size(100), "100B");
        assert_eq!(errors::bash_with_status("", "Command aborted"), "Command aborted");
        assert_eq!(
            errors::bash_with_status("x", &errors::bash_exit(2)),
            "x\n\nCommand exited with code 2"
        );
        assert_eq!(
            errors::notices_suffix(&[errors::ls_limit_notice(500), errors::byte_limit_notice(16384)]),
            "\n\n[500 entries limit reached. Use limit=1000 for more. 16.0KB limit reached]"
        );
        assert_eq!(
            errors::edit_overlap(0, 1, "a.rs"),
            "edits[0] and edits[1] overlap in a.rs. Merge them into one edit or target disjoint regions."
        );
    }
}

/// The `subagent` tool of `builtin:subagent` (E11, oxa-only: the hook set lives in
/// `oxa-core/src/subagent.rs`). ADAPTED from pi's subagent example: the agent scope sentences
/// are replaced by the agent list (project agents load only from a trusted project), and the
/// queue is named. Placeholders in braces are filled by the caller.
pub mod subagent {
    pub const NAME: &str = "subagent";
    pub const DESCRIPTION: &str = "Delegate tasks to specialized subagents with isolated context. Modes: single (agent + task), parallel (tasks array), chain (sequential with {previous} placeholder). Parallel tasks run one after another in call order.";
    /// Appended to [`DESCRIPTION`]: `{list}` = `name (user|project): description; ...` or `none`.
    pub const AVAILABLE_AGENTS: &str = " Available agents: {list}.";
    /// pi's parameter schema without `agentScope` / `confirmProjectAgents` (the agents are the
    /// user's and, in a trusted project, the project's).
    pub const SCHEMA: &str = r#"{"type":"object","properties":{"agent":{"type":"string","description":"Name of the agent to invoke (for single mode)"},"task":{"type":"string","description":"Task to delegate (for single mode)"},"tasks":{"type":"array","description":"Array of {agent, task} for parallel execution","items":{"type":"object","properties":{"agent":{"type":"string","description":"Name of the agent to invoke"},"task":{"type":"string","description":"Task to delegate to the agent"},"cwd":{"type":"string","description":"Working directory for the agent process"}},"required":["agent","task"]}},"chain":{"type":"array","description":"Array of {agent, task} for sequential execution","items":{"type":"object","properties":{"agent":{"type":"string","description":"Name of the agent to invoke"},"task":{"type":"string","description":"Task with optional {previous} placeholder for prior output"},"cwd":{"type":"string","description":"Working directory for the agent process"}},"required":["agent","task"]}},"cwd":{"type":"string","description":"Working directory for the agent process (single mode)"}}}"#;
    /// `{n}` = the number of tasks (pi's text).
    pub const TOO_MANY: &str = "Too many parallel tasks ({n}). Max is 8.";
    /// A chain is the same eight children, one after another (O-146).
    pub const TOO_MANY_CHAIN: &str = "Too many chain steps ({n}). Max is 8.";
    /// The cap is per parent session, over every call in flight.
    pub const TOO_MANY_SESSION: &str = "Too many subagent tasks for this session ({pending} running or queued, {n} more requested). Max is 8. Wait for the running subagent call to finish, or ask for fewer tasks.";
    pub const NOT_ATTACHED: &str = "Subagent failed: the session host is not attached";
    pub const NOT_STARTED: &str = "Subagent was aborted before starting";
    pub const ABORTED: &str = "Subagent was aborted";
    /// `{error}`.
    pub const FAILED: &str = "Subagent failed: {error}";
    /// `{agent}`, `{available}` (`"a", "b"` or `none`).
    pub const UNKNOWN_AGENT: &str = "Unknown agent: \"{agent}\". Available agents: {available}.";
    /// `{error}`.
    pub const SPAWN_FAILED: &str = "Failed to spawn subagent: {error}";
    /// `{signal}`.
    pub const SIGNALLED: &str = "Subagent terminated by signal {signal}";
    /// `{code}`.
    pub const EXITED: &str = "Subagent exited with code {code}";
    /// `{step}`, `{agent}`, `{output}`.
    pub const CHAIN_STOPPED: &str = "Chain stopped at step {step} ({agent}): {output}";
    /// `{ok}`, `{total}`, then the summaries joined by [`PARALLEL_SEPARATOR`].
    pub const PARALLEL_HEADER: &str = "Parallel: {ok}/{total} succeeded\n\n";
    /// One parallel summary: `{agent}`, `{status}`, `{output}`.
    pub const PARALLEL_ITEM: &str = "### [{agent}] {status}\n\n{output}";
    pub const PARALLEL_SEPARATOR: &str = "\n\n---\n\n";
    /// The truncation note of a capped result: `{bytes}`, `{path}`.
    pub const TRUNCATED_SAVED: &str = "[subagent output truncated: {bytes} bytes, full text in {path}]";
    /// `{bytes}`.
    pub const TRUNCATED_NOT_SAVED: &str = "[subagent output truncated: {bytes} bytes, full text not saved]";
    /// pi's `getFailureEvidence` labels (`<label>: <value>`, joined by a blank line).
    pub const EVIDENCE_ERROR: &str = "Error";
    pub const EVIDENCE_SIGNAL: &str = "Signal";
    pub const EVIDENCE_STDERR: &str = "stderr";
    pub const EVIDENCE_OUTPUT: &str = "Assistant output";
    /// A child that produced no text.
    pub const NO_OUTPUT: &str = "(no output)";
    /// `{available}` = the agent list (`name (user|project)`, comma separated, or `none`).
    pub const INVALID_MODE: &str =
        "Invalid parameters. Provide exactly one mode.\nAvailable agents: {available}";
    /// A failed single call: `{why}` (the stop reason or the status), `{output}`.
    pub const SINGLE_FAILED: &str = "Agent {why}: {output}";
    /// The task line of a child's prompt: `{task}`.
    pub const TASK: &str = "Task: {task}";
}

/// The `web_search` tool (wave H lane `search`, O-169; oxa-only, pi has no built-in search):
/// every text the model sees about it. The tool, its provider (Brave Search) and the result
/// shaping live in `oxa-web`. Placeholders in braces are filled by the caller. Every error is
/// the one problem shape `<what>: <why>` / `Fix: <action>`, deterministic (no times, no
/// request ids, never the key).
pub mod web_search {
    pub const NAME: &str = "web_search";
    pub const DESCRIPTION: &str = "Search the web (Brave Search). Returns numbered sources, each with its title, URL and text snippets from the page. Use it for current information or facts that are not in the workspace, and cite the sources you use by URL. The query is sent to Brave Search.";
    /// Strings and integers only (the same schema under both profiles).
    pub const SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string","description":"The search query (at most 600 characters)"},"count":{"type":"integer","description":"Maximum number of sources, 1 to 20 (optional)"},"freshness":{"type":"string","description":"Only pages from: pd (past day), pw (past week), pm (past month), py (past year), or a date range like 2026-01-01to2026-01-31 (optional)","anyOf":[{"enum":["pd","pw","pm","py"]},{"pattern":"^[0-9]{4}-[0-9]{2}-[0-9]{2}to[0-9]{4}-[0-9]{2}-[0-9]{2}$"}]},"country":{"type":"string","description":"Two-letter country code for the results, e.g. us (optional)"},"search_lang":{"type":"string","description":"Language code for the results, e.g. en (optional)"}},"required":["query"]}"#;
    /// The longest query in characters (the provider's limit).
    pub const MAX_QUERY_CHARS: usize = 600;
    /// The largest `count`.
    pub const MAX_COUNT: u64 = 20;

    // ---- the result ----
    /// First line: `{query}`.
    pub const HEADER: &str = "Web search results for: {query}";
    /// One source: `{n}`, `{title}`; the URL follows on its own line.
    pub const SOURCE: &str = "[{n}] {title}";
    /// A source without a title.
    pub const UNTITLED: &str = "(untitled)";
    /// The provider's date of a page: `{age}`.
    pub const AGE: &str = "Published: {age}";
    /// The marker of a snippet cut by the per-snippet budget.
    pub const CUT: &str = " [...]";
    /// `{n}` sources did not fit the budget.
    pub const OMITTED: &str = "({n} more sources omitted)";
    pub const NO_RESULTS: &str = "No results.";

    // ---- argument errors ----
    pub const QUERY_MISSING: &str =
        "Invalid query: it is missing or empty\nFix: pass the search terms as query";
    /// `{n}` = the query's length in characters.
    pub const QUERY_TOO_LONG: &str =
        "Invalid query: it has {n} characters, the limit is 600\nFix: shorten the query to 600 characters or fewer";
    pub const BAD_COUNT: &str = "Invalid count: it must be a whole number from 1 to 20\nFix: pass count between 1 and 20, or leave it out";
    pub const BAD_FRESHNESS: &str = "Invalid freshness: it must be pd (past day), pw (past week), pm (past month), py (past year) or a range YYYY-MM-DDtoYYYY-MM-DD\nFix: pass one of these values, or leave freshness out";
    pub const BAD_COUNTRY: &str = "Invalid country: it must be a two-letter country code such as us\nFix: pass a two-letter code, or leave country out";
    pub const BAD_LANG: &str = "Invalid search_lang: it must be a language code such as en or pt-br\nFix: pass a language code, or leave search_lang out";
    /// `{name}` = an argument the tool does not know.
    pub const UNKNOWN_ARG: &str =
        "Invalid argument {name}: web_search takes query, count, freshness, country and search_lang\nFix: drop {name}";
    /// `{name}` = an argument of the wrong JSON type.
    pub const WRONG_TYPE: &str = "Invalid argument {name}: wrong type\nFix: pass query, freshness, country and search_lang as strings and count as an integer";

    // ---- provider errors ----
    pub const KEY_INVALID: &str = "web_search failed: the Brave Search API key is missing or invalid (HTTP 401/403)\nFix: set BRAVE_SEARCH_API_KEY, or put the key in brave-search.key in the oxa agent directory (~/.oxa/agent, mode 0600)";
    pub const RATE_LIMITED: &str =
        "web_search failed: Brave Search is rate limiting (HTTP 429) after 3 attempts\nFix: wait a moment, then search again";
    /// `{status}`.
    pub const SERVER_ERROR: &str =
        "web_search failed: Brave Search answered HTTP {status} after 3 attempts\nFix: try again later";
    /// `{status}`, `{detail}` (the provider's error text, or `no details`).
    pub const HTTP_ERROR: &str =
        "web_search failed: Brave Search answered HTTP {status} ({detail})\nFix: check the query and the optional parameters";
    pub const NO_DETAIL: &str = "no details";
    pub const TIMEOUT: &str =
        "web_search failed: Brave Search did not answer within 30 seconds\nFix: try again later";
    pub const UNREACHABLE: &str =
        "web_search failed: cannot reach Brave Search\nFix: check the network connection, then search again";
    pub const MALFORMED: &str =
        "web_search failed: Brave Search sent a response that is not valid JSON\nFix: try again later";
    /// A mis-wired test (`OXA_TEST_ISOLATION=1` without a loopback mock).
    pub const TEST_REAL_HOST: &str = "web_search refused: the real Brave Search host is not allowed under OXA_TEST_ISOLATION=1\nFix: point OXA_BRAVE_BASE_URL at a loopback mock server";
}

/// TOOL-DIET and profile `local` texts of the read-class tools (V020-OPTIMIZE #2,
/// OXA-LOCAL-FIRST T8). Lane `l4-read` fills this module; nothing here is used under
/// `ToolTune::default()`.
pub mod diet {
    /// Added inside the first-page notice of `read` in profile `local` (T8), before the `]`.
    pub const READ_PAGE_HINT: &str = " Or use grep to find the part you need.";
    /// The one notice of an over-limit `grep` in profile `local` (T8): the total, the limit,
    /// and how to narrow the search.
    pub fn grep_over_limit_notice(total: usize, limit: usize) -> String {
        format!(
            "{total} matches in total, the limit was {limit}. Narrow the pattern, path or glob, or raise limit, to see the lines."
        )
    }
    /// L4: the second notice of a `grep`, `find` or `ls` answer cut by its byte cap in profile
    /// `local` (after `<cap> limit reached`).
    pub const BYTE_CUT_HINT: &str = "Narrow the search to see the rest";
    /// One file's line in an over-limit `grep` answer: `<path>: <n> matches`.
    pub fn grep_file_count(path: &str, n: usize) -> String {
        format!("{path}: {n} matches")
    }
}

/// Profile `local` texts of `edit`'s failure path and of `bash` (OXA-LOCAL-FIRST T7, F5).
/// Lane `l4-edit` fills this module; nothing here is used under `ToolTune::default()`.
pub mod local_edit {
    /// The most bytes of the closest region shown by [`not_found`].
    pub const CLOSEST_MAX_BYTES: usize = 400;
    /// How many line numbers [`not_unique`] lists before `...`.
    pub const LISTED_LINES: usize = 10;

    /// `first` and `last` are 1-based, inclusive; `region` is the file text shown.
    pub fn not_found(path: &str, first: usize, last: usize, region: &str) -> String {
        format!("Could not find oldText in {path}. The closest text in the file is lines {first}-{last}:\n{region}\nCopy oldText exactly from the file.")
    }
    /// `lines` are the 1-based first lines of the matches, in file order.
    pub fn not_unique(path: &str, lines: &[usize]) -> String {
        let shown: Vec<String> = lines.iter().take(LISTED_LINES).map(|l| l.to_string()).collect();
        let tail = if lines.len() > LISTED_LINES {
            ", ... Add surrounding lines to make it unique."
        } else {
            ". Add surrounding lines to make it unique."
        };
        format!(
            "oldText matches {} places in {path}, at lines {}{tail}",
            lines.len(),
            shown.join(", ")
        )
    }
}

/// Profile `local` declarations of the built-ins (OXA-LOCAL-FIRST T3: integer parameters,
/// required first, one-sentence descriptions). Same names, classes, snippets and guidelines as
/// pi's texts; `edit` is [`super::EDIT_FLAT`].
pub mod local_specs {
    use super::{ToolText, BASH, EDIT_FLAT, FIND, GREP, LS, READ, WRITE};

    pub const READ_LOCAL: ToolText = ToolText {
        name: READ.name,
        description: "Read a text file. Long files are returned one page at a time; continue with offset.",
        schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"File path (relative or absolute)"},"offset":{"type":"integer","description":"First line to read (1-indexed)"},"limit":{"type":"integer","description":"Maximum number of lines"}},"required":["path"]}"#,
        class: READ.class,
        prompt_snippet: READ.prompt_snippet,
        guidelines: READ.guidelines,
    };

    pub const BASH_LOCAL: ToolText = ToolText {
        name: BASH.name,
        description: "Run a bash command in the working directory. Returns stdout and stderr. Output over 16KB is cut to its first and last part and the full output is saved to a file.",
        schema_json: r#"{"type":"object","properties":{"command":{"type":"string","description":"Command to run"},"timeout":{"type":"integer","description":"Timeout in seconds"}},"required":["command"]}"#,
        class: BASH.class,
        prompt_snippet: BASH.prompt_snippet,
        guidelines: BASH.guidelines,
    };

    pub const WRITE_LOCAL: ToolText = ToolText {
        name: WRITE.name,
        description: "Write a file, creating parent directories. Overwrites an existing file.",
        schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"File path (relative or absolute)"},"content":{"type":"string","description":"The whole file content"}},"required":["path","content"]}"#,
        class: WRITE.class,
        prompt_snippet: WRITE.prompt_snippet,
        guidelines: WRITE.guidelines,
    };

    pub const GREP_LOCAL: ToolText = ToolText {
        name: GREP.name,
        description: "Search file contents for a regex. Returns matching lines with path and line number. Respects .gitignore. At most 100 matches unless limit is set.",
        schema_json: r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Regex (a literal string with literal=true)"},"path":{"type":"string","description":"Directory or file to search (default: working directory)"},"glob":{"type":"string","description":"Only files matching this glob, e.g. '*.rs' or 'src/**/*.ts'"},"ignoreCase":{"type":"boolean","description":"Case-insensitive search"},"literal":{"type":"boolean","description":"Treat pattern as a literal string"},"context":{"type":"integer","description":"Lines to show before and after each match"},"limit":{"type":"integer","description":"Maximum number of matches"}},"required":["pattern"]}"#,
        class: GREP.class,
        prompt_snippet: GREP.prompt_snippet,
        guidelines: GREP.guidelines,
    };

    pub const FIND_LOCAL: ToolText = ToolText {
        name: FIND.name,
        description: "Find files by glob pattern. Returns paths relative to the search directory. Respects .gitignore. At most 1000 results unless limit is set.",
        schema_json: r#"{"type":"object","properties":{"pattern":{"type":"string","description":"Glob, e.g. '*.rs' or 'src/**/*.ts'"},"path":{"type":"string","description":"Directory to search (default: working directory)"},"limit":{"type":"integer","description":"Maximum number of results"}},"required":["pattern"]}"#,
        class: FIND.class,
        prompt_snippet: FIND.prompt_snippet,
        guidelines: FIND.guidelines,
    };

    pub const LS_LOCAL: ToolText = ToolText {
        name: LS.name,
        description: "List a directory. Directories end with '/'. Dotfiles are included. At most 500 entries unless limit is set.",
        schema_json: r#"{"type":"object","properties":{"path":{"type":"string","description":"Directory to list (default: working directory)"},"limit":{"type":"integer","description":"Maximum number of entries"}}}"#,
        class: LS.class,
        prompt_snippet: LS.prompt_snippet,
        guidelines: LS.guidelines,
    };

    /// The profile `local` text of a built-in, by name; `None` for any other name.
    pub fn by_name(name: &str) -> Option<&'static ToolText> {
        match name {
            "read" => Some(&READ_LOCAL),
            "bash" => Some(&BASH_LOCAL),
            "edit" => Some(&EDIT_FLAT),
            "write" => Some(&WRITE_LOCAL),
            "grep" => Some(&GREP_LOCAL),
            "find" => Some(&FIND_LOCAL),
            "ls" => Some(&LS_LOCAL),
            _ => None,
        }
    }
}
