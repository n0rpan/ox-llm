//! The review task builder (plan item 25, batch 2): a pure description of a review task.
//!
//! Two things a review run needs before any model is involved: the `git` argument list for the
//! diff to review ([`diff_argv`]) and the text that carries that diff to a reviewer together with
//! its rubric ([`prompt`]). Both are pure: no process is started, no settings are read, nothing is
//! registered as a tool or a command. A caller builds the argv, runs `git` itself and passes the
//! output back; the frozen prefix is unchanged.
//!
//! ## Refusal
//! A revision from user input must never become an option: a reference that starts with `-`, is
//! empty, or contains a control character (a newline would split one argument, a NUL cannot be an
//! argument at all) is refused with [`codes::INVALID`] instead of being passed to `git`.
//!
//! ## Caps
//! A diff of a large change is bigger than a request may be. [`prompt`] keeps at most
//! [`PER_FILE_CAP`] bytes per file and at most `limit` diff bytes in total, cutting on a line
//! boundary (on a character boundary when a single line is longer than the cap) and saying so in
//! a visible marker, so a reviewer can tell a one-file change from a trimmed one. The rubric and
//! the markers are not counted against `limit`: the instructions are never cut.
use oxa_proto::{codes, OxaError};

/// What the diff is taken against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Unstaged changes of the work tree against the index (`git diff`). Staged changes are in
    /// `Selection::Base("HEAD")`; an unborn `HEAD` is still reviewable here.
    Uncommitted,
    /// Everything since `reference`, e.g. `origin/main` or `HEAD~3` (`git diff <reference>`).
    Base(String),
    /// One commit as `git show` prints it (`--format=` drops the commit message): its change
    /// against its first parent, or against the empty tree for a root commit.
    Commit(String),
}

/// Bytes of one file's diff kept before it is cut ([`prompt`]).
pub const PER_FILE_CAP: usize = 16 * 1024;

/// The `git` argument list (program excluded) for the diff `selection` names.
///
/// `--no-ext-diff` and `--no-textconv` keep a repository's own configuration from starting a
/// program instead of printing the diff, and the trailing `--` ends the revisions so nothing
/// after them can be read as a path (or as an option).
pub fn diff_argv(selection: &Selection) -> Result<Vec<String>, OxaError> {
    let mut argv = match selection {
        Selection::Uncommitted => vec!["diff".to_string()],
        Selection::Base(reference) => vec!["diff".to_string(), checked("base", reference)?],
        Selection::Commit(commit) => {
            vec!["show".to_string(), "--format=".to_string(), checked("commit", commit)?]
        }
    };
    argv.push("--no-ext-diff".to_string());
    argv.push("--no-textconv".to_string());
    argv.push("--".to_string());
    Ok(argv)
}

/// One revision argument, or the refusal for something that cannot be a revision.
fn checked(what: &str, revision: &str) -> Result<String, OxaError> {
    if revision.is_empty() {
        return Err(refused(what, revision, "it is empty"));
    }
    if revision.starts_with('-') {
        return Err(refused(what, revision, "it starts with '-'"));
    }
    if revision.chars().any(char::is_control) {
        return Err(refused(what, revision, "it contains a control character"));
    }
    Ok(revision.to_string())
}

fn refused(what: &str, revision: &str, why: &str) -> OxaError {
    OxaError::new(
        codes::INVALID,
        format!("the review {what} {revision:?} is not a git revision: {why}"),
    )
    .with_hint("name a branch, tag or commit, e.g. origin/main or 3eb8ad14")
}

/// The text that asks for the review of `diff` under `rubric`: the rubric first, then the diff
/// cut to at most [`PER_FILE_CAP`] bytes per file and `limit` bytes in total.
///
/// `limit` is the budget of diff bytes, not of the whole text: the rubric and the cut markers
/// are always kept, so the result is longer than `limit` by what the markers take.
pub fn prompt(diff: &str, rubric: &str, limit: usize) -> String {
    let mut out = String::new();
    if !rubric.is_empty() {
        out.push_str(rubric.trim_end());
        out.push_str("\n\n");
    }
    let mut kept = 0usize;
    let all = sections(diff);
    for (n, (title, text)) in all.iter().enumerate() {
        let room = limit.saturating_sub(kept);
        let per_file = clip(text, PER_FILE_CAP).0;
        let shown = clip(per_file, room).0;
        if !shown.is_empty() {
            out.push_str(shown);
            if !shown.ends_with('\n') {
                out.push('\n');
            }
            kept += shown.len();
        }
        if shown.len() == text.len() {
            continue;
        }
        out.push_str(&file_mark(title, shown.len(), text.len()));
        if shown.len() < per_file.len() {
            out.push_str(&total_mark(limit, all.len() - n - 1));
            break;
        }
    }
    out
}

/// The `(title, bytes)` of each file in a `git diff`, in order. `title` is the header line
/// without `diff --git `; text before the first header (or a diff without headers) is the
/// unnamed first section.
fn sections(diff: &str) -> Vec<(String, &str)> {
    const HEAD: &str = "diff --git ";
    let mut starts = Vec::new();
    if diff.starts_with(HEAD) {
        starts.push(0);
    }
    for (at, _) in diff.match_indices("\ndiff --git ") {
        starts.push(at + 1);
    }
    if starts.is_empty() {
        return vec![(String::new(), diff)];
    }
    let mut out = Vec::new();
    if starts[0] > 0 {
        out.push((String::new(), &diff[..starts[0]]));
    }
    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(diff.len());
        let head_end = diff[start..end].find('\n').map_or(end, |i| start + i);
        let title = diff[start + HEAD.len()..head_end].to_string();
        out.push((title, &diff[start..end]));
    }
    out
}

/// The first `cap` bytes of `text`, on a line boundary when there is one (a character boundary
/// otherwise, so a multi-byte character is never split), and the number of bytes left out.
fn clip(text: &str, cap: usize) -> (&str, usize) {
    if text.len() <= cap {
        return (text, 0);
    }
    let mut cut = cap;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    if let Some(nl) = text[..cut].rfind('\n') {
        cut = nl + 1;
    }
    (&text[..cut], text.len() - cut)
}

fn file_mark(title: &str, kept: usize, total: usize) -> String {
    let name = if title.is_empty() { "the diff" } else { title };
    format!("[... oxa: {name} cut: kept {kept} of {total} bytes ...]\n")
}

fn total_mark(limit: usize, omitted: usize) -> String {
    let files = if omitted == 1 { "file" } else { "files" };
    format!(
        "[... oxa: diff cut at the {limit} byte total limit; {omitted} more {files} not shown ...]\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_files() -> String {
        let line = " line of a diff that is repeated to grow one file past the cap\n";
        let big = line.repeat(PER_FILE_CAP / line.len() + 8);
        format!(
            "diff --git a/one.rs b/one.rs\n--- a/one.rs\n+++ b/one.rs\n@@ -1 +1 @@\n{big}\
             diff --git a/two.txt b/two.txt\n--- a/two.txt\n+++ b/two.txt\n@@ -1 +1 @@\n-two\n+3\n"
        )
    }

    #[test]
    fn uncommitted_is_a_plain_diff() {
        assert_eq!(
            diff_argv(&Selection::Uncommitted).unwrap(),
            ["diff", "--no-ext-diff", "--no-textconv", "--"]
        );
    }

    #[test]
    fn base_names_the_reference() {
        assert_eq!(
            diff_argv(&Selection::Base("origin/main".to_string())).unwrap(),
            ["diff", "origin/main", "--no-ext-diff", "--no-textconv", "--"]
        );
    }

    #[test]
    fn commit_is_a_show_without_the_message() {
        assert_eq!(
            diff_argv(&Selection::Commit("3eb8ad14".to_string())).unwrap(),
            ["show", "--format=", "3eb8ad14", "--no-ext-diff", "--no-textconv", "--"]
        );
    }

    #[test]
    fn an_option_or_a_control_character_is_refused() {
        for bad in ["-p", "--output=/tmp/x", "", "HEAD\n--output=/tmp/x", "ma\u{7}in"] {
            for selection in [Selection::Base(bad.to_string()), Selection::Commit(bad.to_string())] {
                let e = diff_argv(&selection).unwrap_err();
                assert_eq!(e.code, codes::INVALID, "{bad:?}");
            }
        }
    }

    #[test]
    fn a_fitting_diff_is_kept_whole_and_uncut() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let out = prompt(diff, "Review it.", 4096);
        assert_eq!(out, format!("Review it.\n\n{diff}"));
        assert!(!out.contains("cut"));
    }

    #[test]
    fn a_long_file_is_cut_per_file_with_a_visible_marker_and_keeps_the_next_file() {
        let out = prompt(&two_files(), "", usize::MAX);
        assert!(out.contains("diff --git a/one.rs"), "the first file is shown");
        assert!(out.contains("diff --git a/two.txt"), "the second file is still shown");
        assert!(out.contains("cut: kept "), "the cut is visible");
        assert!(!out.contains("total limit"), "the per-file cap is enough");
        let first = out.split("diff --git a/two.txt").next().unwrap();
        let kept: usize = first
            .lines()
            .filter(|l| l.starts_with(" line of a diff"))
            .map(|l| l.len() + 1)
            .sum();
        assert!(kept <= PER_FILE_CAP, "kept {kept} bytes of one file");
        assert!(kept >= PER_FILE_CAP - 128, "the cut is close to the cap: {kept}");
    }

    #[test]
    fn the_total_limit_stops_after_the_last_whole_file_and_says_how_many_are_left() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+b\n\
                    diff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-c\n+d\n\
                    diff --git a/c.rs b/c.rs\n--- a/c.rs\n+++ b/c.rs\n@@ -1 +1 @@\n-e\n+f\n";
        let first = diff.split("diff --git a/b.rs").next().unwrap();
        let out = prompt(diff, "", first.len());
        assert!(out.contains("diff --git a/a.rs"));
        assert!(out.contains("1 more file not shown"), "{out}");
        assert!(!out.contains("total limit; 2"), "the counted file is the one cut");
        assert!(!out.contains("diff --git a/c.rs"), "nothing fits after the limit");
    }

    #[test]
    fn a_zero_limit_keeps_only_the_markers() {
        let out = prompt(&two_files(), "Rubric.", 0);
        assert!(out.starts_with("Rubric.\n\n"));
        assert!(out.contains("cut: kept 0 of "));
        assert!(out.contains("total limit"));
        assert_eq!(out.matches("diff --git").count(), 1, "the first file's header is the marker's text");
    }

    #[test]
    fn the_rubric_is_not_cut_by_the_diff_limit() {
        let rubric = "Check every hunk.\n";
        let out = prompt("diff --git a/a.rs b/a.rs\n+x\n", rubric, 0);
        assert!(out.starts_with(rubric), "{out}");
    }

    #[test]
    fn a_multibyte_line_is_cut_on_a_character_boundary() {
        let diff = format!("diff --git a/u.rs b/u.rs\n+{}\n", "é".repeat(PER_FILE_CAP));
        let out = prompt(&diff, "", usize::MAX);
        assert!(out.contains("cut: kept "), "the long line is cut");
        assert!(out.ends_with("...]\n"), "the marker closes the text");
        let kept = out.split("[... oxa:").next().unwrap();
        assert!(kept.len() <= PER_FILE_CAP + "diff --git a/u.rs b/u.rs\n+".len());
        assert!(!kept.ends_with('é') || kept.len() % 2 == 0, "no half character");
    }

    #[test]
    fn a_diff_without_git_headers_is_one_unnamed_section() {
        let out = prompt("plain patch text\n", "", 4096);
        assert_eq!(out, "plain patch text\n");
    }

    #[test]
    fn an_empty_diff_leaves_the_rubric_alone() {
        assert_eq!(prompt("", "Rubric.", 100), "Rubric.");
    }
}
