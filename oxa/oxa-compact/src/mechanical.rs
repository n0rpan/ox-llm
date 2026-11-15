//! OXA-LOCAL-FIRST R4: the mechanical summary, built without a model call when a compaction's
//! summary could not be made. A pure function of the summarised messages and
//! the file lists (W4: same input, same bytes); every text piece is in `prompt.rs`.
use crate::prompt;
use oxa_proto::{Role, WireMsg};

/// The summary text (file lists included) for the messages being summarised (`wire[..cut]`):
/// [`prompt::MECHANICAL_SUMMARY_HEADER`]; the text of a previous compaction summary at the head
/// of `summarised` (its own file lists left out: `read` / `modified` carry them); the user
/// messages in order, each cut to [`prompt::MECHANICAL_PROMPT_MAX_CHARS`] characters; the file
/// lists ([`prompt::file_lists_block`]); the number of tool results left out.
pub fn mechanical_summary(summarised: &[WireMsg], read: &[String], modified: &[String]) -> String {
    let mut s = String::from(prompt::MECHANICAL_SUMMARY_HEADER);
    let mut msgs = summarised;
    if let Some(prev) = summarised.first().and_then(summary_head_text) {
        msgs = &summarised[1..];
        s.push_str("\n\n");
        s.push_str(prompt::MECHANICAL_EARLIER_HEADING);
        s.push('\n');
        s.push_str(without_file_lists(prev));
    }
    s.push_str("\n\n");
    s.push_str(prompt::MECHANICAL_PROMPTS_HEADING);
    let prompts: Vec<&str> = msgs
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| m.content.as_deref())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    if prompts.is_empty() {
        s.push('\n');
        s.push_str(prompt::MECHANICAL_NO_PROMPTS);
    }
    for (i, p) in prompts.iter().enumerate() {
        s.push_str(&format!("\n{}. {}", i + 1, cut_chars(p)));
    }
    s.push_str(&prompt::file_lists_block(read, modified));
    s.push_str("\n\n");
    let tools = msgs.iter().filter(|m| m.role == Role::Tool).count();
    s.push_str(&prompt::mechanical_elided(tools));
    s
}

/// At most [`prompt::MECHANICAL_PROMPT_MAX_CHARS`] characters (cut at a char boundary), with
/// [`prompt::MECHANICAL_CUT_MARK`] when cut.
fn cut_chars(text: &str) -> String {
    match text.char_indices().nth(prompt::MECHANICAL_PROMPT_MAX_CHARS) {
        None => text.to_string(),
        Some((at, _)) => format!("{}{}", &text[..at], prompt::MECHANICAL_CUT_MARK),
    }
}

/// The summary inside a compaction summary message, `None` for any other message.
fn summary_head_text(m: &WireMsg) -> Option<&str> {
    if m.role != Role::User {
        return None;
    }
    m.content
        .as_deref()?
        .strip_prefix(prompt::COMPACTION_SUMMARY_PREFIX)?
        .strip_suffix(prompt::COMPACTION_SUMMARY_SUFFIX)
}

/// A summary text without the trailing [`prompt::file_lists_block`] it may end with.
fn without_file_lists(summary: &str) -> &str {
    let ends_with_lists = summary.ends_with("\n</read-files>") || summary.ends_with("\n</modified-files>");
    if !ends_with_lists {
        return summary;
    }
    let at = ["\n\n<read-files>\n", "\n\n<modified-files>\n"]
        .iter()
        .filter_map(|marker| summary.rfind(marker))
        .min();
    match at {
        Some(at) => &summary[..at],
        None => summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_at_char_boundary() {
        let long = "é".repeat(prompt::MECHANICAL_PROMPT_MAX_CHARS + 3);
        let cut = cut_chars(&long);
        assert!(cut.ends_with(prompt::MECHANICAL_CUT_MARK));
        assert_eq!(
            cut.trim_end_matches(prompt::MECHANICAL_CUT_MARK).chars().count(),
            prompt::MECHANICAL_PROMPT_MAX_CHARS
        );
        let exact = "a".repeat(prompt::MECHANICAL_PROMPT_MAX_CHARS);
        assert_eq!(cut_chars(&exact), exact);
    }

    #[test]
    fn strips_trailing_file_lists_only() {
        let s = format!("S{}", prompt::file_lists_block(&["a".into()], &["b".into()]));
        assert_eq!(without_file_lists(&s), "S");
        assert_eq!(without_file_lists("plain"), "plain");
    }
}
