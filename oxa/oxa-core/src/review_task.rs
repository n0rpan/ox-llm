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
//! [`PER_FILE_CAP`] bytes per file and at most `limit` diff bytes in total, ending on a line
//! boundary where one is close behind the cut and cutting inside the line on a character
//! boundary otherwise (a minified file is one long line; keeping a piece of it beats keeping
//! none), and saying so in a visible marker, so a reviewer can tell a one-file change from a
//! trimmed one. The rubric and the markers are not counted against `limit`: the instructions are
//! never cut.
use oxa_proto::{codes, OxaError};

/// What the diff is taken against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Unstaged changes of the work tree against the index (`git diff`).
    Uncommitted,
    /// Staged changes, including the initial index before the first commit (`git diff --cached`).
    Staged,
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
/// program instead of printing the diff. Colour and submodule-ignore settings cannot hide patch
/// headers or gitlink changes. The trailing `--` ends the revisions so nothing
/// after them can be read as a path (or as an option).
pub fn diff_argv(selection: &Selection) -> Result<Vec<String>, OxaError> {
    let mut argv = match selection {
        Selection::Uncommitted => vec!["diff".to_string()],
        Selection::Staged => vec!["diff".to_string(), "--cached".to_string()],
        Selection::Base(reference) => vec!["diff".to_string(), checked("base", reference)?],
        Selection::Commit(commit) => {
            let commit = checked("commit", commit)?;
            if commit.starts_with('^')
                || commit.contains("..")
                || commit.contains("^@")
                || commit.contains("^!")
                || commit.contains("^-")
                || commit.contains(':')
                || commit.chars().any(char::is_whitespace)
            {
                return Err(refused(
                    "commit",
                    &commit,
                    "name one commit, not a revision set or path",
                ));
            }
            vec![
                "show".to_string(),
                "--format=".to_string(),
                "--root".to_string(),
                "--diff-merges=first-parent".to_string(),
                format!("{commit}^{{commit}}"),
            ]
        }
    };
    argv.push("--no-ext-diff".to_string());
    argv.push("--no-textconv".to_string());
    argv.push("--no-color".to_string());
    argv.push("--ignore-submodules=none".to_string());
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
/// are always kept, so the result is longer than `limit` by what the markers take. The rubric
/// stands alone, without a trailing blank line, when there is no diff text to show under it.
pub fn prompt(diff: &str, rubric: &str, limit: usize) -> String {
    let text = cut_diff(diff, limit);
    let rubric = rubric.trim_end();
    if text.is_empty() {
        return rubric.to_string();
    }
    if rubric.is_empty() {
        return text;
    }
    format!("{rubric}\n\n{text}")
}

/// The diff cut to `limit` bytes in total and [`PER_FILE_CAP`] per file, with a visible marker
/// wherever something was left out.
fn cut_diff(diff: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut kept = 0usize;
    let all = sections(diff);
    for (n, (title, text)) in all.iter().enumerate() {
        let room = limit.saturating_sub(kept);
        let (per_file, _) = clip(text, PER_FILE_CAP);
        let (shown, _) = clip(per_file, room);
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
            out.push_str(&total_mark(limit, files_after(&all, n)));
            break;
        }
    }
    out
}

/// The `(title, bytes)` of each file in a `git diff`, in order. `title` is the header line
/// without `diff --git `; text before the first header (or a diff without headers) is the
/// unnamed first section, which is a piece of a file's diff rather than one of the files after
/// the cut and so never counts as a file left out.
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

/// How many files follow section `n` in `all`.
fn files_after(all: &[(String, &str)], n: usize) -> usize {
    let after = all[n + 1..].iter().filter(|(title, _)| !title.is_empty());
    after.count()
}

/// The first `cap` bytes of `text`, and the number of bytes left out. The cut ends on a line
/// boundary when one is within a quarter of `cap` behind it (or a quarter of what is left when
/// `cap` is tiny); otherwise it ends on a character boundary, so a multi-byte character is never
/// split and a single line longer than `cap` still shows its start instead of nothing.
fn clip(text: &str, cap: usize) -> (&str, usize) {
    if text.len() <= cap {
        return (text, 0);
    }
    let mut cut = cap;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let slack = (cap.min(text.len()) / 4).max(1);
    match text[..cut].rfind('\n') {
        Some(nl) if cut - (nl + 1) <= slack => cut = nl + 1,
        _ => {}
    }
    (&text[..cut], text.len() - cut)
}

fn file_mark(title: &str, kept: usize, total: usize) -> String {
    let name = if title.is_empty() { "the diff" } else { title };
    format!("[... oxa: {name} cut: kept {kept} of {total} bytes ...]\n")
}

fn total_mark(limit: usize, omitted: usize) -> String {
    let files = if omitted == 1 { "file" } else { "files" };
    let cut = format!("[... oxa: diff cut at the {limit} byte total limit;");
    format!("{cut} {omitted} more {files} not shown ...]\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(env: &oxa_fake::TestEnv, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(env.work())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", env.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn repository() -> oxa_fake::TestEnv {
        let env = oxa_fake::TestEnv::new();
        git(&env, &["init", "-b", "main"]);
        git(&env, &["config", "user.name", "Test"]);
        git(&env, &["config", "user.email", "test@example.invalid"]);
        env
    }

    fn selected_diff(env: &oxa_fake::TestEnv, selection: Selection) -> String {
        let args = diff_argv(&selection).unwrap();
        git(env, &args.iter().map(String::as_str).collect::<Vec<_>>())
    }

    #[test]
    fn merge_commit_review_includes_first_parent_changes() {
        let env = repository();
        env.write("work/base.txt", "base\n");
        git(&env, &["add", "."]);
        git(&env, &["commit", "-m", "base"]);
        git(&env, &["checkout", "-b", "topic"]);
        env.write("work/topic.txt", "topic\n");
        git(&env, &["add", "."]);
        git(&env, &["commit", "-m", "topic"]);
        git(&env, &["checkout", "main"]);
        git(&env, &["merge", "--no-ff", "topic", "-m", "merge"]);
        let diff = selected_diff(&env, Selection::Commit("HEAD".into()));
        assert!(diff.contains("diff --git a/topic.txt b/topic.txt"), "{diff}");
        assert!(diff.contains("+topic"), "{diff}");
    }

    #[test]
    fn root_and_annotated_tag_reviews_ignore_display_configuration() {
        let env = repository();
        env.write("work/root.txt", "root\n");
        git(&env, &["add", "."]);
        git(&env, &["commit", "-m", "root"]);
        git(
            &env,
            &[
                "tag",
                "-a",
                "review-tag",
                "-m",
                "annotation must not enter review",
            ],
        );
        git(&env, &["config", "log.showRoot", "false"]);
        git(&env, &["config", "color.ui", "always"]);
        git(&env, &["config", "color.diff", "always"]);
        let diff = selected_diff(&env, Selection::Commit("review-tag".into()));
        assert!(diff.starts_with("diff --git a/root.txt b/root.txt"), "{diff}");
        assert!(diff.contains("+root"), "{diff}");
        assert!(!diff.contains("annotation must not enter review"), "{diff}");
        assert!(!diff.contains('\u{1b}'), "{diff:?}");
    }

    #[test]
    fn submodule_ignore_configuration_cannot_hide_a_changed_gitlink() {
        let env = repository();
        env.write("work/base.txt", "base\n");
        git(&env, &["add", "."]);
        git(&env, &["commit", "-m", "base"]);
        let commit = git(&env, &["rev-parse", "HEAD"]);
        let entry = format!("160000,{},module", commit.trim());
        git(&env, &["update-index", "--add", "--cacheinfo", &entry]);
        std::fs::create_dir(env.work().join("module")).unwrap();
        git(&env, &["config", "diff.ignoreSubmodules", "all"]);
        let diff = selected_diff(&env, Selection::Base("HEAD".into()));
        assert!(diff.contains("diff --git a/module b/module"), "{diff}");
        assert!(diff.contains("+Subproject commit"), "{diff}");
    }

    #[test]
    fn commit_selection_refuses_revision_sets() {
        for revision in [
            "HEAD~2..HEAD",
            "HEAD...main",
            "^HEAD",
            "HEAD^@",
            "HEAD^!",
            "HEAD^-2",
        ] {
            assert!(
                diff_argv(&Selection::Commit(revision.into())).is_err(),
                "{revision}"
            );
        }
    }

    #[test]
    fn base_selection_refuses_revision_sets() {
        for revision in ["HEAD~2..HEAD", "HEAD...main", "^HEAD", "HEAD^@", "HEAD^!"] {
            assert!(
                diff_argv(&Selection::Base(revision.into())).is_err(),
                "{revision}"
            );
        }
    }

    #[test]
    fn review_from_a_subdirectory_includes_files_outside_it() {
        let env = repository();
        env.write("work/a/one.txt", "one\n");
        env.write("work/b/two.txt", "two\n");
        git(&env, &["add", "."]);
        git(&env, &["commit", "-m", "root"]);
        git(&env, &["config", "diff.relative", "true"]);
        let mut args = vec!["-C".to_string(), "a".to_string()];
        args.extend(diff_argv(&Selection::Commit("HEAD".into())).unwrap());
        let diff = git(&env, &args.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(diff.contains("diff --git a/a/one.txt b/a/one.txt"), "{diff}");
        assert!(diff.contains("diff --git a/b/two.txt b/b/two.txt"), "{diff}");
    }

    #[test]
    fn combined_diff_files_keep_independent_caps() {
        for header in ["diff --cc ", "diff --combined "] {
            let big = " synthetic conflict line\n".repeat(PER_FILE_CAP / 20);
            let diff = format!("{header}one.rs\n{big}{header}two.rs\n+  second file\n");
            let result = prompt(&diff, "", usize::MAX);
            assert!(
                result.contains(&format!("{header}two.rs")),
                "second conflict must survive the first file cap"
            );
            assert!(result.contains("+  second file"));
            assert!(result.contains("one.rs cut:"));
        }
    }

    #[test]
    fn commit_review_does_not_launch_a_configured_signature_verifier() {
        use std::os::unix::fs::PermissionsExt;
        let env = repository();
        env.write("work/root.txt", "root\n");
        git(&env, &["add", "."]);
        git(&env, &["commit", "-m", "root"]);
        let tree = git(&env, &["rev-parse", "HEAD^{tree}"]);
        let parent = git(&env, &["rev-parse", "HEAD"]);
        // Deliberately synthetic signature text: no key, signing or external service.
        env.write("work/synthetic-commit", &format!(
            "tree {}\nparent {}\nauthor Test <test@example.invalid> 1 +0000\ncommitter Test <test@example.invalid> 1 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n synthetic\n -----END PGP SIGNATURE-----\n\nsynthetic commit\n",
            tree.trim(), parent.trim()
        ));
        let commit = git(&env, &["hash-object", "-t", "commit", "-w", "synthetic-commit"]);
        let verifier = env.write(
            "work/verifier",
            "#!/bin/sh\nprintf called > verifier-called\nexit 1\n",
        );
        std::fs::set_permissions(&verifier, std::fs::Permissions::from_mode(0o700)).unwrap();
        git(&env, &["config", "gpg.program", verifier.to_str().unwrap()]);
        git(&env, &["config", "log.showSignature", "true"]);
        selected_diff(&env, Selection::Commit(commit.trim().into()));
        assert!(
            !env.work().join("verifier-called").exists(),
            "a diff review must not execute the configured verifier"
        );
    }

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
            [
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--ignore-submodules=none",
                "--"
            ]
        );
    }

    #[test]
    fn staged_changes_are_reviewable_before_the_first_commit() {
        let env = repository();
        env.write("work/initial.txt", "initial\n");
        git(&env, &["add", "."]);
        let diff = selected_diff(&env, Selection::Staged);
        assert!(diff.contains("diff --git a/initial.txt b/initial.txt"), "{diff}");
        assert!(diff.contains("+initial"), "{diff}");
    }

    #[test]
    fn base_names_the_reference() {
        assert_eq!(
            diff_argv(&Selection::Base("origin/main".to_string())).unwrap(),
            [
                "diff",
                "origin/main",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--ignore-submodules=none",
                "--"
            ]
        );
    }

    #[test]
    fn commit_is_a_show_without_the_message() {
        assert_eq!(
            diff_argv(&Selection::Commit("3eb8ad14".to_string())).unwrap(),
            [
                "show",
                "--format=",
                "--root",
                "--diff-merges=first-parent",
                "3eb8ad14^{commit}",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--ignore-submodules=none",
                "--"
            ]
        );
    }

    #[test]
    fn an_option_or_a_control_character_is_refused() {
        let bases = ["-p", "--output=/tmp/x", "", "HEAD\n--output=/tmp/x", "ma\u{7}in"];
        for bad in bases {
            let base = Selection::Base(bad.to_string());
            let commit = Selection::Commit(bad.to_string());
            for selection in [base, commit] {
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
        assert!(
            out.contains("diff --git a/two.txt"),
            "the second file is still shown"
        );
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
        assert!(
            !out.contains("diff --git"),
            "no file text is shown at a zero limit: {out}"
        );
        assert!(
            out.contains("a/one.rs b/one.rs"),
            "the marker names the file it cut: {out}"
        );
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
    fn text_before_the_first_header_is_not_a_file_left_out() {
        let diff = "stray text\n\
                    diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+b\n\
                    diff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n@@ -1 +1 @@\n-c\n+d\n";
        // The budget is the stray text plus the first file's header line: the cut lands inside
        // the first file, so the count names the second file only — the stray text is not a file.
        let stray = diff.find("diff --git a/a.rs").unwrap();
        let header = diff[stray..].split('\n').next().unwrap().len() + 1;
        let out = prompt(diff, "", stray + header);
        assert!(out.contains("diff --git a/a.rs"), "the first file is shown");
        assert!(out.contains("1 more file not shown"), "{out}");
        assert!(!out.contains("2 more files"), "{out}");
    }

    #[test]
    fn a_line_longer_than_the_budget_still_shows_its_start() {
        let diff = format!("diff --git a/m.js b/m.js\n+{}\n", "x".repeat(PER_FILE_CAP * 2));
        let out = prompt(&diff, "", 64);
        assert!(out.contains("cut: kept "), "the cut is visible: {out}");
        assert!(out.contains("total limit"), "{out}");
        let shown = out.split("[... oxa:").next().unwrap();
        assert!(shown.starts_with("diff --git a/m.js"), "{out}");
        assert!(shown.len() <= diff.len() / 2, "{out}");
    }

    #[test]
    fn an_empty_diff_leaves_the_rubric_alone() {
        assert_eq!(prompt("", "Rubric.", 100), "Rubric.");
    }
}
