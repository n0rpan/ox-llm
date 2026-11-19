//! Issue #1007: a session's task packet asked for `gh pr view` / `gh pr checks` output, and its own
//! rules said GitHub API calls belong to the outside runner and `gh` must not be invoked from the
//! session's tools. Neither instruction could be dropped, so the pull request's reviews and check
//! results were either fetched against the rules or never read.
//!
//! The repository guidance assigns workers' GitHub access to their runner. It treats a packet's
//! fetch line as provenance and says what a worker hands over. Sessions that own GitHub access
//! can still use the documented `gh` commands.

const GUIDANCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../AGENTS.md");
const RULE: &str = "GitHub access has one owner per session.";
const SCOPE: &str = "Below, every `gh` command is for a session that owns GitHub access.";

/// The guidance with its line breaks flattened, so a rewrapped paragraph still matches.
fn guidance() -> String {
    let text = std::fs::read_to_string(GUIDANCE).unwrap_or_else(|e| panic!("{GUIDANCE}: {e}"));
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The rule paragraph, from its heading to the sentence that scopes the `gh` commands below it.
fn rule(text: &str) -> &str {
    let start = text
        .find(RULE)
        .expect("the guidance has no rule for a session whose packet asks for pull request or check output");
    let tail = &text[start..];
    let end = tail
        .find(SCOPE)
        .map(|i| i + SCOPE.len())
        .expect("the rule has to scope the `gh` commands that follow it");
    &tail[..end]
}

#[test]
fn a_packet_line_asking_for_pull_request_output_is_not_an_order() {
    let text = guidance();
    let rule = rule(&text);
    assert!(
        rule.contains("Their runner owns GitHub access."),
        "the worker's runner must own GitHub access: {rule}"
    );
    assert!(
        rule.contains("make no `gh` call"),
        "a session without GitHub access must not call gh for reviews or checks: {rule}"
    );
    assert!(
        rule.contains("provenance, not an order"),
        "a fetch line inside a supplied packet is provenance: {rule}"
    );
}

#[test]
fn a_session_without_github_access_hands_over_the_commit_and_the_missing_fact() {
    let text = guidance();
    let rule = rule(&text);
    assert!(
        rule.contains("the branch, the base and the exact head commit"),
        "the hand-over names the exact commit the owner has to look at: {rule}"
    );
    assert!(
        rule.contains("which fact is missing"),
        "a fact the packet leaves out is reported, not fetched: {rule}"
    );
    assert!(
        rule.contains("CI result"),
        "the pull request body's CI result is left to the session that owns GitHub: {rule}"
    );
}

#[test]
fn sessions_that_own_github_keep_the_lane_commands_below_the_rule() {
    let text = guidance();
    let at = text.find(RULE).expect("the GitHub ownership rule is missing");
    for command in [
        "gh workflow run lane --ref <branch>",
        "gh run watch <run-id> --interval 60 --exit-status",
        "gh run view <run-id> --log-failed",
    ] {
        let found = text
            .find(command)
            .unwrap_or_else(|| panic!("{command} is missing from the guidance"));
        assert!(
            at < found,
            "{command} has to follow the rule that scopes it to a session owning GitHub"
        );
    }
}
