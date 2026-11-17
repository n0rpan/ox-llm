//! Issue #1089: a session text told the model both to read an issue with `gh` and to leave GitHub
//! to the outside runner that launched it, so the two rules pulled in opposite directions and the
//! agent had to pick one. The repository guidance names the owner of GitHub access once and scopes
//! every `gh` command in this file to a session that owns it, so a task packet cannot turn into a
//! second, opposite instruction.

const GUIDANCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../AGENTS.md");

/// The guidance with its line breaks flattened, so a rewrapped paragraph still matches.
fn guidance() -> String {
    let text = std::fs::read_to_string(GUIDANCE).unwrap_or_else(|e| panic!("{GUIDANCE}: {e}"));
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

const RULE: &str = "GitHub access has one owner per session.";

#[test]
fn the_session_prompt_decides_who_owns_github() {
    let text = guidance();
    let start = text
        .find(RULE)
        .expect("the guidance has no single GitHub-access rule");
    let tail = &text[start..];
    let scoped = "session that owns GitHub access.";
    let end = tail
        .find(scoped)
        .map(|i| i + scoped.len())
        .expect("the rule has to scope the `gh` commands in this file");
    let rule = &tail[..end];
    assert!(
        rule.contains("The session prompt says which; the two never apply at once."),
        "the rule has to name the deciding instruction: {rule}"
    );
    assert!(
        rule.contains("make no `gh` call at all"),
        "a runner-owned session must not call gh: {rule}"
    );
    assert!(
        rule.contains("provenance and not an instruction to fetch it"),
        "a line saying how the packet was fetched is not an order to fetch it: {rule}"
    );
    assert!(
        rule.contains("reported as missing instead of fetched"),
        "a missing fact is reported, not fetched: {rule}"
    );
}

#[test]
fn every_gh_command_in_the_guidance_is_scoped_by_that_rule() {
    let text = guidance();
    let rule = text.find(RULE).expect("the ownership rule is missing");
    let no_call = text
        .find("make no `gh` call at all")
        .expect("the no-call sentence is missing");
    for command in [
        "gh workflow run lane",
        "gh run watch <run-id>",
        "gh run view <run-id>",
    ] {
        let at = text
            .find(command)
            .unwrap_or_else(|| panic!("{command} is missing from the guidance"));
        assert!(
            rule < no_call && no_call < at,
            "{command} has to follow the ownership rule that scopes it to a session owning GitHub"
        );
    }
}
