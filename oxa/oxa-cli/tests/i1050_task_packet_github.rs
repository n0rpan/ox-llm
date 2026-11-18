//! Issue #1050: a task packet showed how it had been fetched (`gh issue view 1050 ...`) and, further
//! down, said GitHub access belongs to the outside runner and `gh` must not be invoked. The agent
//! had to choose between a line it could not follow and the rule that forbade it, so the metadata
//! read was dropped.
//!
//! The repository guidance assigns access to the runner, says the fetch line is provenance
//! rather than an order, and says a fact the packet leaves out is reported instead of fetched. It
//! also scopes every `gh` command in the file to a session that owns GitHub access, so the two
//! directions cannot disagree again.

const GUIDANCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../AGENTS.md");
const RULE: &str = "GitHub access has one owner per session.";

/// The guidance with its line breaks flattened, so rewrapping a paragraph still matches.
fn guidance() -> String {
    let text = std::fs::read_to_string(GUIDANCE).unwrap_or_else(|e| panic!("{GUIDANCE}: {e}"));
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The rule paragraph, from its heading to the sentence that scopes the `gh` commands below it.
fn rule(text: &str) -> &str {
    let start = text
        .find(RULE)
        .expect("the guidance has no rule for a packet line that says how it was fetched");
    let tail = &text[start..];
    let scoped = "Below, every `gh` command is for a session that owns GitHub access.";
    let end = tail
        .find(scoped)
        .map(|i| i + scoped.len())
        .expect("the rule has to scope the `gh` commands that follow it");
    &tail[..end]
}

#[test]
fn a_fetch_line_in_a_runner_owned_packet_is_not_an_order() {
    let text = guidance();
    let rule = rule(&text);
    assert!(
        rule.contains("Their runner owns GitHub access."),
        "the rule has to assign GitHub access to the worker's runner: {rule}"
    );
    assert!(
        rule.contains("provenance, not an order"),
        "a packet's fetch line must not authorize a GitHub call: {rule}"
    );
    assert!(
        rule.contains("make no `gh` call"),
        "a runner-owned session must not call gh: {rule}"
    );
    assert!(
        rule.contains("reported as missing instead of fetched"),
        "a missing fact is reported, not fetched: {rule}"
    );
}

#[test]
fn the_lane_commands_follow_the_rule_that_scopes_them() {
    let text = guidance();
    let at = text.find(RULE).expect("the ownership rule is missing");
    for command in [
        "gh workflow run lane",
        "gh run watch <run-id>",
        "gh run view <run-id>",
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
