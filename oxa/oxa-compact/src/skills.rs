//! C2: the skills active before a compaction, listed in its summary so the agent re-reads them.
//! A skill is loaded as a user message opening with `<skill name=".." location="..">`
//! (`oxa-resources`, `expand_skill_command`). The compaction summarises that message, so the
//! pairs are carried the way the file lists are: from the summarised user messages, and from
//! the `activeSkills` details of earlier compactions.
use std::collections::BTreeSet;

use oxa_proto::{Entry, EntryBody, Role, Value};

/// The opening of the `<skill>` tag: the name attribute starts right after it.
const TAG_OPEN: &str = "<skill name=\"";
/// Starts the block appended to a summary (after its file lists).
const BLOCK_OPEN: &str = "\n\n<active-skills>\n";
/// Ends the block; a summary ends with it when the block is there.
const BLOCK_CLOSE: &str = "\n</active-skills>";
/// The last line of the block, before [`BLOCK_CLOSE`].
const RE_READ: &str = "Re-read these files before continuing.";

/// The `(name, location)` pairs of the skills active in `branch`: the `<skill>` tags of the
/// user messages of `branch`, plus the `activeSkills` carried in the `details` of earlier
/// `Compaction` and `BranchSummary` entries. Sorted and de-duplicated.
pub fn active_skills(branch: &[&Entry]) -> Vec<(String, String)> {
    let mut skills: BTreeSet<(String, String)> = BTreeSet::new();
    for e in branch {
        match &e.body {
            EntryBody::Msg { wire, .. } if wire.role == Role::User => {
                if let Some(text) = wire.content.as_deref() {
                    skills.extend(skill_tags(text));
                }
            }
            EntryBody::Compaction {
                details,
                from_hook: false,
                ..
            }
            | EntryBody::BranchSummary {
                details,
                from_hook: false,
                ..
            } => {
                for item in details
                    .get("activeSkills")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let (Some(name), Some(location)) = (
                        item.get("name").and_then(Value::as_str),
                        item.get("location").and_then(Value::as_str),
                    ) {
                        skills.insert((name.to_owned(), location.to_owned()));
                    }
                }
            }
            _ => {}
        }
    }
    skills.into_iter().collect()
}

/// The `(name, location)` of each `<skill name=".." location="..">` opening tag in `text`.
fn skill_tags(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(TAG_OPEN) {
        rest = &rest[at + TAG_OPEN.len()..];
        let Some(name_end) = rest.find('"') else {
            break;
        };
        let name = &rest[..name_end];
        let Some(after_name) = rest[name_end..].strip_prefix("\" location=\"") else {
            continue;
        };
        let Some(loc_end) = after_name.find('"') else {
            break;
        };
        let location = &after_name[..loc_end];
        if !name.is_empty() && !location.is_empty() && after_name[loc_end..].starts_with("\">") {
            out.push((name.to_owned(), location.to_owned()));
        }
        rest = &after_name[loc_end..];
    }
    out
}

/// The block listing `skills` (one `name location` line each) for a summary text, starting with
/// a blank line like the file lists. Empty when there are no skills.
pub fn active_skills_block(skills: &[(String, String)]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = skills
        .iter()
        .map(|(name, location)| format!("{name} {location}"))
        .collect();
    format!("{BLOCK_OPEN}{}\n{RE_READ}{BLOCK_CLOSE}", lines.join("\n"))
}

/// The block listing `skills` within `budget` tokens of the summary: whole `name location` lines
/// are kept in order while they fit, and the ones left out are counted, never silently cut. The
/// elision line is only written when at least one line was kept. Empty when nothing fits, like
/// [`active_skills_block`] with no skills.
pub fn active_skills_block_within(
    skills: &[(String, String)],
    budget: u32,
    count: impl Fn(&str) -> u32,
) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0u32;
    for (name, location) in skills {
        let line = format!("{name} {location}");
        let cost = count(&line);
        if used.saturating_add(cost) > budget {
            break;
        }
        used = used.saturating_add(cost);
        kept.push(line);
    }
    if kept.is_empty() {
        return String::new();
    }
    let left = skills.len() - kept.len();
    if left > 0 {
        kept.push(format!("({left} more not listed: summary budget)"));
    }
    format!("{BLOCK_OPEN}{}\n{RE_READ}{BLOCK_CLOSE}", kept.join("\n"))
}

/// A summary text without the trailing [`active_skills_block`] it may end with.
pub(crate) fn without_active_skills(summary: &str) -> &str {
    if !summary.ends_with(BLOCK_CLOSE) {
        return summary;
    }
    match summary.rfind(BLOCK_OPEN) {
        Some(at) => &summary[..at],
        None => summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_skill_tags_only() {
        let text = "<skill name=\"lint\" location=\"/s/lint/SKILL.md\">\nbody\n</skill>\n\nrun";
        assert_eq!(
            skill_tags(text),
            vec![("lint".to_string(), "/s/lint/SKILL.md".to_string())]
        );
        assert!(skill_tags("<skill name=\"x\">no location</skill>").is_empty());
        assert!(skill_tags("plain text").is_empty());
    }

    #[test]
    fn block_lists_skills_and_strips_back() {
        assert_eq!(active_skills_block(&[]), "");
        let skills = [("lint".to_string(), "/s/lint/SKILL.md".to_string())];
        let block = active_skills_block(&skills);
        assert_eq!(
            block,
            "\n\n<active-skills>\nlint /s/lint/SKILL.md\nRe-read these files before continuing.\n</active-skills>"
        );
        assert_eq!(without_active_skills(&format!("S{block}")), "S");
        assert_eq!(without_active_skills("plain"), "plain");
    }

    /// The counter the session uses, one token per byte (the fake's template).
    fn bytes(text: &str) -> u32 {
        u32::try_from(text.len()).unwrap_or(u32::MAX)
    }

    #[test]
    fn budgeted_block_keeps_whole_lines_and_counts_the_rest() {
        let skills: Vec<(String, String)> = (0..5)
            .map(|i| (format!("s{i}"), format!("/s/s{i}/SKILL.md")))
            .collect();
        // Two lines fit in 40 bytes (each is 17), the third does not.
        let block = active_skills_block_within(&skills, 40, bytes);
        assert!(block.contains("s0 /s/s0/SKILL.md"), "{block}");
        assert!(block.contains("s1 /s/s1/SKILL.md"), "{block}");
        assert!(!block.contains("s2 "), "{block}");
        assert!(block.contains("(3 more not listed: summary budget)"), "{block}");
        assert!(block.ends_with("Re-read these files before continuing.\n</active-skills>"));
        // Nothing fits: no block at all, like a session with no skills.
        assert_eq!(active_skills_block_within(&skills, 5, bytes), "");
        // Everything fits: the plain block, no elision line.
        assert_eq!(
            active_skills_block_within(&skills, 10_000, bytes),
            active_skills_block(&skills)
        );
    }
}
