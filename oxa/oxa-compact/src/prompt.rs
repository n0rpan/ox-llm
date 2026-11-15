// ported from pi packages/coding-agent/src/core/compaction/{compaction,branch-summarization,utils}.ts and packages/coding-agent/src/core/messages.ts
//! W0-FROZEN (architect-owned): every text of compaction and branch summaries. Tasks B1, B2,
//! C3 and C4 use these; they never write a summary literal of their own.
//!
//! Source: pi 1.5.2 @47e9d74, verbatim unless marked `ADAPTED`.
//!
//! **How oxa uses them (W5).** pi sends a *separate* request: a summarizer system prompt plus
//! the conversation serialised as text. oxa sends the **whole wire + one user instruction**
//! (same prefix, same tools, thinking off, background), so the request is a pure append and
//! reuses the cache. Therefore:
//! * the instruction is one of [`SUMMARIZATION_PROMPT`], [`UPDATE_SUMMARIZATION_PROMPT`],
//!   [`BRANCH_SUMMARY_PROMPT`], headed by [`SUMMARY_INSTRUCTION_HEADER`] (see
//!   [`summary_instruction`]);
//! * pi's [`SUMMARIZATION_SYSTEM_PROMPT`] cannot be a system message (W3); its two sentences
//!   are the header of the instruction instead;
//! * pi's `<conversation>` serialisation ([`serialize`]) is used only where the text is not
//!   already on the wire: a branch summary of an *abandoned* branch is still an append to
//!   that branch's wire, so it is not needed there either; it stays for tests.

/// pi's summarizer system prompt. In oxa its text is carried by [`SUMMARY_INSTRUCTION_HEADER`].
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// ADAPTED (oxa): first lines of the one user instruction of a summary request. Says what
/// pi's system prompt says, addressed to the same assistant, and forbids tool calls (the
/// tools stay declared because the prefix is frozen).
pub const SUMMARY_INSTRUCTION_HEADER: &str = "Stop working on the task now. Your next reply is a context summary, not a continuation: read the conversation above and produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. Do NOT call tools. ONLY output the structured summary.";

/// Initial compaction instruction (no previous summary on the wire).
pub const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// pi's update rules + format (used inside [`UPDATE_SUMMARIZATION_PROMPT`]).
pub const UPDATE_SUMMARIZATION_INSTRUCTIONS: &str = r#"Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// ADAPTED first sentence: pi says "the existing summary provided in <previous-summary>
/// tags"; in oxa the previous summary is the first user message of the wire, wrapped in
/// `<summary>` tags by [`summary_user_message`]. Used for a repeated compaction.
pub const UPDATE_SUMMARIZATION_LEAD: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <summary> tags at the start of the conversation.";

/// pi's lead, verbatim (for PARITY notes).
pub const PI_UPDATE_SUMMARIZATION_LEAD: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.";

/// pi's split-turn prompt. oxa replaces the split-turn double summary by the W5 mid-run path
/// (plan sec. 1); kept verbatim for PARITY's A-row and for a future use.
pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = r#"The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.

Create a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.

## Original Request
[What did the user ask for?]

## Progress So Far
- [Key decisions and work completed in these messages]

## Context Needed to Continue
- [Information from these messages needed to understand the later work]

Only summarize information explicitly present above. Do not infer or recreate later messages."#;

/// Branch summary instruction.
pub const BRANCH_SUMMARY_PROMPT: &str = r#"Create a structured summary of this conversation branch for context when returning later.

Use this EXACT format:

## Goal
[What was the user trying to accomplish in this branch?]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Work that was started but not finished]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [What should happen next to continue this work]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Prepended by pi to the generated branch summary text before it is stored.
pub const BRANCH_SUMMARY_PREAMBLE: &str =
    "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

/// Wrapper of a compaction summary when it re-enters context as a user message.
pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Wrapper of a branch summary when it enters context as a user message. (pi's suffix has no
/// leading newline, unlike the compaction one.)
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// A1: the text that replaces an oversized tool result in the kept tail of a compaction (the
/// summary request saw the whole result). `bytes` = the size of the result it replaces.
pub fn elided_tool_result(bytes: usize) -> String {
    format!(
        "[output elided at compaction: {bytes} bytes; the summary above covers it. Run the tool again if the exact text is needed.]"
    )
}

/// #459: the text that replaces an older `read` result in the kept tail of a compaction when
/// the tail reads the same page of `path` again or edits/writes it later.
pub fn superseded_read(path: &str) -> String {
    format!("[older read of {path} dropped at compaction: a later read or edit below supersedes it.]")
}

/// How custom instructions (`/compact <instr>`, `/tree` focus) extend an instruction.
pub const ADDITIONAL_FOCUS: &str = "\n\nAdditional focus: ";

/// pi's failure texts (`{label}` = `Compaction` / `Branch summary`).
pub fn summary_failed(label: &str, error: Option<&str>) -> String {
    format!("{label} failed: {}", error.unwrap_or("Unknown error"))
}
pub fn summary_hit_length(label: &str) -> String {
    format!("{label} failed: generation hit the token cap and the summary is incomplete")
}

/// Maximum characters of one tool result in [`serialize`] (pi `TOOL_RESULT_MAX_CHARS`).
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Which summary an instruction is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryKind {
    /// First compaction of a wire.
    Initial,
    /// The wire already starts with a compaction summary.
    Update,
    Branch,
}

/// The one user instruction appended to the whole wire for a summary request (W5):
/// header, blank line, the pi instruction, then `Additional focus: <custom>` if given.
pub fn summary_instruction(kind: SummaryKind, custom: Option<&str>) -> String {
    summary_instruction_with_target(kind, custom, 0)
}

/// V020-OPTIMIZE #7: [`summary_instruction`] with a soft length target. `target_words` = 0 is
/// exactly [`summary_instruction`]; otherwise [`summary_length_target`] follows the pi
/// instruction after a blank line (before `Additional focus`), still in the one user message.
/// The target is soft: the request's `max_tokens` does not change.
pub fn summary_instruction_with_target(kind: SummaryKind, custom: Option<&str>, target_words: u32) -> String {
    let mut s = String::from(SUMMARY_INSTRUCTION_HEADER);
    s.push_str("\n\n");
    match kind {
        SummaryKind::Initial => s.push_str(SUMMARIZATION_PROMPT),
        SummaryKind::Update => {
            s.push_str(UPDATE_SUMMARIZATION_LEAD);
            s.push_str("\n\n");
            s.push_str(UPDATE_SUMMARIZATION_INSTRUCTIONS);
        }
        SummaryKind::Branch => s.push_str(BRANCH_SUMMARY_PROMPT),
    }
    if target_words > 0 {
        s.push_str("\n\n");
        s.push_str(&summary_length_target(target_words));
    }
    if let Some(c) = custom.map(str::trim).filter(|c| !c.is_empty()) {
        s.push_str(ADDITIONAL_FOCUS);
        s.push_str(c);
    }
    s
}

/// V020-OPTIMIZE #7: the soft length target sentence of a summary instruction.
pub fn summary_length_target(words: u32) -> String {
    format!("Keep the whole summary to at most about {words} words.")
}

/// V020-OPTIMIZE #7: the word count asked for by a summary target of `target_tokens` tokens:
/// three quarters of it, rounded to the nearest multiple of 50 (halves up), at least 50; 0 when
/// there is no target.
pub fn summary_target_words(target_tokens: u32) -> u32 {
    if target_tokens == 0 {
        return 0;
    }
    let words = u64::from(target_tokens) * 3 / 4;
    let rounded = (words + 25) / 50 * 50;
    u32::try_from(rounded.max(50)).unwrap_or(u32::MAX)
}

/// OXA-LOCAL-FIRST R4: first line of a mechanical summary (built without the model when no
/// summary could be made).
pub const MECHANICAL_SUMMARY_HEADER: &str = "[Automatic record: no summary could be made, so this record was built without the model. It lists the user's messages in order and the files touched. Assistant replies and tool results are not included.]";

/// R4: heading of the previous summary carried into a mechanical summary.
pub const MECHANICAL_EARLIER_HEADING: &str = "## Earlier summary";

/// R4: heading of the list of user messages of a mechanical summary.
pub const MECHANICAL_PROMPTS_HEADING: &str = "## User messages";

/// R4: the list line when there were no user messages.
pub const MECHANICAL_NO_PROMPTS: &str = "(none)";

/// R4: each user message of a mechanical summary is cut to this many characters.
pub const MECHANICAL_PROMPT_MAX_CHARS: usize = 300;

/// R4: the marker appended to a user message cut to [`MECHANICAL_PROMPT_MAX_CHARS`].
pub const MECHANICAL_CUT_MARK: &str = " [...]";

/// R4: the last line of a mechanical summary.
pub fn mechanical_elided(tool_results: usize) -> String {
    format!("Tool results left out: {tool_results}.")
}

/// The user message that carries a compaction summary at the head of the rebased wire.
/// `summary` already ends with [`file_lists_block`] when there are files.
pub fn summary_user_message(summary: &str) -> String {
    format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}")
}

/// The user message that carries a branch summary at the new leaf. `summary` is the stored
/// text (it starts with [`BRANCH_SUMMARY_PREAMBLE`]).
pub fn branch_summary_user_message(summary: &str) -> String {
    format!("{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}")
}

/// pi's `formatFileOperations`: appended to a summary text. Empty when both lists are empty;
/// otherwise `\n\n` + `<read-files>` block and / or `<modified-files>` block, separated by a
/// blank line. Callers pass sorted lists; `read` excludes files that are also modified
/// (pi's `computeFileLists`).
pub fn file_lists_block(read: &[String], modified: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read.is_empty() {
        sections.push(format!("<read-files>\n{}\n</read-files>", read.join("\n")));
    }
    if !modified.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

/// One message for [`serialize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line<'a> {
    User(&'a str),
    Assistant(&'a str),
    /// Already formatted as `name(k=<json>, ..)`; several calls joined by `; `.
    ToolCalls(&'a str),
    ToolResult(&'a str),
}

/// pi's `serializeConversation`: `[User]: ..`, `[Assistant]: ..`, `[Assistant tool calls]: ..`,
/// `[Tool result]: ..` joined by blank lines; empty texts are skipped; a tool result longer
/// than [`TOOL_RESULT_MAX_CHARS`] characters keeps its head plus
/// `\n\n[... N more characters truncated]`. Thinking is omitted.
pub fn serialize(lines: &[Line]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for l in lines {
        match *l {
            Line::User(t) if !t.is_empty() => parts.push(format!("[User]: {t}")),
            Line::Assistant(t) if !t.is_empty() => parts.push(format!("[Assistant]: {t}")),
            Line::ToolCalls(t) if !t.is_empty() => parts.push(format!("[Assistant tool calls]: {t}")),
            Line::ToolResult(t) if !t.is_empty() => {
                let n = t.chars().count();
                if n <= TOOL_RESULT_MAX_CHARS {
                    parts.push(format!("[Tool result]: {t}"));
                } else {
                    let head: String = t.chars().take(TOOL_RESULT_MAX_CHARS).collect();
                    parts.push(format!(
                        "[Tool result]: {head}\n\n[... {} more characters truncated]",
                        n - TOOL_RESULT_MAX_CHARS
                    ));
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_match_pi() {
        assert_eq!(
            summary_user_message("S"),
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\nS\n</summary>"
        );
        assert!(branch_summary_user_message("B").ends_with("<summary>\nB</summary>"));
    }

    #[test]
    fn file_lists() {
        assert_eq!(file_lists_block(&[], &[]), "");
        assert_eq!(
            file_lists_block(&["a".into(), "b".into()], &["c".into()]),
            "\n\n<read-files>\na\nb\n</read-files>\n\n<modified-files>\nc\n</modified-files>"
        );
        assert_eq!(
            file_lists_block(&[], &["c".into()]),
            "\n\n<modified-files>\nc\n</modified-files>"
        );
    }

    #[test]
    fn instruction_forms() {
        let i = summary_instruction(SummaryKind::Initial, None);
        assert!(i.starts_with(SUMMARY_INSTRUCTION_HEADER) && i.ends_with(SUMMARIZATION_PROMPT));
        let u = summary_instruction(SummaryKind::Update, Some(" tests "));
        assert!(u.contains(UPDATE_SUMMARIZATION_LEAD) && u.ends_with("\n\nAdditional focus: tests"));
        assert!(summary_instruction(SummaryKind::Branch, Some("  ")).ends_with(BRANCH_SUMMARY_PROMPT));
    }

    #[test]
    fn serialize_truncates_tool_results() {
        let long = "x".repeat(TOOL_RESULT_MAX_CHARS + 5);
        let s = serialize(&[
            Line::User("u"),
            Line::Assistant(""),
            Line::ToolCalls("read(path=\"a\")"),
            Line::ToolResult(&long),
        ]);
        assert!(s.starts_with("[User]: u\n\n[Assistant tool calls]: read(path=\"a\")\n\n[Tool result]: x"));
        assert!(s.ends_with("\n\n[... 5 more characters truncated]"));
    }
}
