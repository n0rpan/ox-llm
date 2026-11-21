//! #2292: the preservation nudge of the dangerous-command floor. Pure text helpers, no
//! policy and no wiring: when the user explicitly asked to keep work, the existing question
//! of a destructive command gains one reversible-checkpoint sentence. Nothing here asks,
//! refuses or caps anything new, and no decision or grant key changes.

/// The longest quote of the user's requirement carried into a question (chars).
const MAX_QUOTE: usize = 160;

/// How many characters before a phrase a negation ("no need to keep") is still read.
const NEGATION_WINDOW: usize = 24;

/// Phrases that explicitly ask to keep work, matched case-insensitively on word boundaries.
/// A curly apostrophe is read as a straight one. This is a heuristic nudge, not a parser: a
/// false positive only adds one sentence to a question the floor already asks.
const PHRASES: &[&str] = &[
    "preserve",
    "preserves",
    "preserving",
    "preservation",
    "keep",
    "keeps",
    "keeping",
    "intact",
    "must survive",
    "must remain",
    "must stay",
    "don't delete",
    "do not delete",
    "don't remove",
    "do not remove",
    "don't touch",
    "do not touch",
    "don't lose",
    "do not lose",
    "without losing",
];

/// Prefixes that turn a phrase into the opposite ("no need to keep the old branches").
const NEGATIONS: &[&str] = &[
    "no need to",
    "no need of",
    "not need to",
    "never need to",
    "don't have to",
    "do not have to",
];

/// The first explicit preservation requirement of `text`: the trimmed line that asks to
/// keep work, quoted and bounded by [`MAX_QUOTE`]. `None` when no line asks. Only the
/// caller's own user text is read here; the session strips its environment note first.
pub fn requirement(text: &str) -> Option<String> {
    let text = text.replace('\u{2019}', "'");
    for line in text.lines() {
        let line = line.trim();
        let lower = line.to_ascii_lowercase();
        let mut best: Option<usize> = None;
        for phrase in PHRASES {
            let mut from = 0;
            while let Some(found) = lower[from..].find(phrase) {
                let at = from + found;
                from = at + phrase.len();
                if !word_match(&lower, at, phrase) || negated(&lower[..at]) {
                    continue;
                }
                best = Some(best.map_or(at, |b| b.min(at)));
                break;
            }
        }
        if let Some(at) = best {
            return Some(quote(line, at));
        }
    }
    None
}

/// The rules whose hit destroys state the user may depend on. The nudge rides on these and
/// on the owner's own `cmd:` rules (a custom rule is how `git filter-repo` or `terraform
/// destroy` joins the floor). The other rules (privilege, piping, parsing, publication) are
/// not about keeping work and keep their reason unchanged.
pub fn destructive(rule: &str) -> bool {
    matches!(
        rule,
        "rm-recursive-outside" | "git-push-force" | "disk-destroy" | "sql-drop"
    ) || rule.starts_with("cmd:")
}

/// The one sentence appended to a destructive hit's existing reason when a preservation
/// requirement is tracked. Additive: the grant key, the answers and the decision are
/// unchanged.
pub(crate) fn checkpoint_note(requirement: &str) -> String {
    format!(
        "the user asked to preserve work (\"{requirement}\"); make a reversible checkpoint first (backup branch/ref, copy or dry run) and check that the kept objects survive this step"
    )
}

/// A phrase matches only on word boundaries, so `keep` is not read inside `keepsake` or
/// `keep_commit`.
fn word_match(lower: &str, at: usize, phrase: &str) -> bool {
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let before = lower[..at].chars().next_back();
    let after = lower[at + phrase.len()..].chars().next();
    !before.is_some_and(word) && !after.is_some_and(word)
}

/// `true` when the text just before the phrase negates it ("no need to keep ...").
fn negated(prefix: &str) -> bool {
    let start = prefix
        .char_indices()
        .rev()
        .take(NEGATION_WINDOW)
        .last()
        .map_or(0, |(i, _)| i);
    NEGATIONS.iter().any(|n| prefix[start..].contains(n))
}

/// The line as the quote, or a bounded window around the phrase when the line is long.
/// Cuts are on character boundaries and marked with an ellipsis.
fn quote(line: &str, at: usize) -> String {
    if line.chars().count() <= MAX_QUOTE {
        return line.to_string();
    }
    let mut start = at.saturating_sub(40);
    while start > 0 && !line.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + MAX_QUOTE).min(line.len());
    while end < line.len() && !line.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&line[start..end]);
    if end < line.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_requirements_are_quoted() {
        assert_eq!(
            requirement("remove the secret, keep commit abc123 exactly as it is").as_deref(),
            Some("remove the secret, keep commit abc123 exactly as it is")
        );
        assert_eq!(
            requirement("Please PRESERVE the data/rows.csv file.\nThen clean up.").as_deref(),
            Some("Please PRESERVE the data/rows.csv file.")
        );
        assert_eq!(
            requirement("don’t delete the release branch").as_deref(),
            Some("don't delete the release branch")
        );
        assert_eq!(
            requirement("the migration must survive\nnext line").as_deref(),
            Some("the migration must survive")
        );
    }

    #[test]
    fn no_requirement_without_an_explicit_phrase() {
        assert_eq!(requirement("clean up the repository"), None);
        assert_eq!(requirement("rewrite history with git filter-repo"), None);
        // Word boundaries: a phrase inside another word is not a requirement.
        assert_eq!(requirement("the keepsake box and keep_commit variable"), None);
        // A negated phrase asks for the opposite.
        assert_eq!(requirement("no need to keep the old branches"), None);
        assert_eq!(requirement("you do not need to preserve the drafts"), None);
    }

    #[test]
    fn long_lines_are_bounded_around_the_phrase() {
        let line = format!("{} keep commit abc123 {}", "x".repeat(300), "y".repeat(300));
        let quote = requirement(&line).expect("requirement");
        assert!(quote.chars().count() <= MAX_QUOTE + 2, "{quote}");
        assert!(quote.contains("keep commit abc123"), "{quote}");
        assert!(quote.starts_with('…') && quote.ends_with('…'), "{quote}");
        // A multibyte character at the cut is never split.
        let line = format!("{} keep it {}", "é".repeat(100), "é".repeat(100));
        let quote = requirement(&line).expect("requirement");
        assert!(quote.contains("keep it"), "{quote}");
    }

    #[test]
    fn destructive_rules_and_custom_rules_carry_the_nudge() {
        for rule in [
            "rm-recursive-outside",
            "git-push-force",
            "disk-destroy",
            "sql-drop",
            "cmd:git filter-repo",
        ] {
            assert!(destructive(rule), "{rule}");
        }
        for rule in [
            "sudo",
            "pipe-to-shell",
            "credential-write",
            "unparsed",
            "publish-unsafe",
            "kill-foreign",
        ] {
            assert!(!destructive(rule), "{rule}");
        }
    }
}
