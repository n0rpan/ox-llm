//! The built-in `pull-request` skill measures the branch's own change, not base drift.
//!
//! A branch that started before its base moved reports the base's own progress in any diff taken
//! from the start commit, so a four-file change reads as thousands of deleted lines. The skill is
//! the workflow the model reads for pull-request work, so it names the merge-base range.

use oxa_resources::BUILTIN_SKILLS;

fn pull_request_skill() -> &'static str {
    BUILTIN_SKILLS
        .iter()
        .find(|(name, _)| *name == "pull-request")
        .map(|(_, text)| *text)
        .expect("the built-in pull-request skill")
}

/// One line per sentence, so an assertion cannot depend on where a line wraps.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn the_own_change_is_measured_from_the_merge_base() {
    let text = pull_request_skill();
    let flat = flat(text);
    assert!(
        flat.contains("git diff $(git merge-base origin/<base> HEAD) HEAD"),
        "the skill gives the merge-base range for the branch's own change: {flat}"
    );
    assert!(
        flat.contains("drift"),
        "the skill says why the start-commit range misleads: {flat}"
    );
    // The range is a quick way, not another step that every session must run: it stays in the
    // branch/commit guidance, and a session that may push is not asked for anything more.
    let own = flat
        .find("merge-base")
        .expect("the own-diff guidance is in the skill");
    let identity = flat
        .find("git config user.name")
        .expect("the commit identity guidance stays");
    assert!(
        own < identity,
        "the own-diff guidance lives with the branch and commit rules"
    );
}
