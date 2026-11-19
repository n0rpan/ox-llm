//! L4-SUBVERIFY: the verification rules of the frozen prefix. With the output diet on, a parent
//! that declares `subagent` gets [`RULE_SUBAGENT_CHECK`] right after [`RULE_SUBAGENT`], and a
//! subagent child gets [`RULE_CHILD_VERIFY`] after the diet rules. Diet-off prefixes get
//! neither. The options are passed directly (no process environment).

use oxa_config::{Dirs, Settings};
use oxa_core::{build_prefix_with, resolve_toolset, PrefixOpts, ToolSelection};
use oxa_fake::TestEnv;
use oxa_proto::{Exposure, Source, ToolClass, ToolSpec};
use oxa_resources::{discover, DiscoverOpts};

const RULE_SUBAGENT: &str =
    "For a wide search or to read many files, delegate to a subagent and use its summary";
const RULE_SUBAGENT_CHECK: &str =
    "A subagent's summary can be wrong: before you rely on a claim from it for an edit, read the cited lines yourself";
const RULE_CHILD_VERIFY: &str = "Before you answer, check every claim about the code with a tool; give path:line for each, and write \"not verified\" for anything you did not check";
const DIET_LAST: &str =
    "- Final answer: at most 6 lines unless asked for more; never paste file contents back";

fn subagent_spec() -> ToolSpec {
    ToolSpec {
        name: "subagent".to_string(),
        description: "Delegate tasks to specialized subagents with isolated context.".to_string(),
        schema_json:
            r#"{"type":"object","properties":{"task":{"type":"string","description":"Task to delegate"}}}"#
                .to_string(),
        class: ToolClass::Exec,
        exposure: Exposure::Direct,
        source: Source::Builtin,
    }
}

/// The system text of a session with the diet on or off, as a parent (`child` false) or a
/// subagent child, with or without a declared `subagent` tool.
fn system(env: &TestEnv, diet_output: bool, child: bool, subagent: bool) -> String {
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), true);
    let opts = DiscoverOpts {
        trusted: true,
        ..Default::default()
    };
    let resources = discover(&dirs, &env.work(), &settings, &opts);
    let (active, _) = resolve_toolset(&oxa_tools::builtin_tools(), &ToolSelection::default());
    let mut specs = active.specs();
    if subagent {
        specs.push(subagent_spec());
    }
    let prefix_opts = PrefixOpts { diet_output, child };
    build_prefix_with(&env.work(), &resources, &specs, &[], &prefix_opts).system_text()
}

/// The line index of `rule` in `system`, if it is a `- ` rule line.
fn line_of(system: &str, rule: &str) -> Option<usize> {
    let want = format!("- {rule}");
    system.lines().position(|l| l == want)
}

#[test]
fn rule_texts_are_the_pinned_wording() {
    use oxa_core::prompt::{RULE_CHILD_VERIFY as C, RULE_SUBAGENT as S, RULE_SUBAGENT_CHECK as K};
    assert_eq!(S, RULE_SUBAGENT);
    assert_eq!(K, RULE_SUBAGENT_CHECK);
    assert_eq!(C, RULE_CHILD_VERIFY);
}

#[test]
fn parent_with_subagent_gets_the_check_right_after_the_delegation_rule() {
    let env = TestEnv::new();
    let s = system(&env, true, false, true);
    let at = line_of(&s, RULE_SUBAGENT).expect("delegation rule present");
    assert_eq!(
        s.lines().nth(at + 1),
        Some(format!("- {RULE_SUBAGENT_CHECK}").as_str())
    );
    assert!(!s.contains(RULE_CHILD_VERIFY), "a parent carries no child rule");
}

#[test]
fn parent_without_subagent_has_no_check() {
    let env = TestEnv::new();
    let s = system(&env, true, false, false);
    assert!(!s.contains(RULE_SUBAGENT_CHECK));
    assert!(!s.contains(RULE_SUBAGENT));
    assert!(!s.contains(RULE_CHILD_VERIFY));
}

#[test]
fn child_gets_the_verify_rule_after_the_diet_rules() {
    let env = TestEnv::new();
    let s = system(&env, true, true, false);
    let at = line_of(&s, RULE_CHILD_VERIFY).expect("child rule present");
    assert_eq!(s.lines().nth(at - 1), Some(DIET_LAST));
    assert!(
        !s.contains(RULE_SUBAGENT_CHECK),
        "a child declares no subagent tool"
    );
}

#[test]
fn a_non_child_session_has_no_verify_rule() {
    let env = TestEnv::new();
    let s = system(&env, true, false, false);
    assert!(!s.contains(RULE_CHILD_VERIFY));
    assert!(s.contains(DIET_LAST));
}

#[test]
fn diet_off_has_neither_rule() {
    let env = TestEnv::new();
    for child in [false, true] {
        for subagent in [false, true] {
            let s = system(&env, false, child, subagent);
            assert!(
                !s.contains(RULE_CHILD_VERIFY),
                "diet off child={child} subagent={subagent}"
            );
            assert!(
                !s.contains(RULE_SUBAGENT_CHECK),
                "diet off child={child} subagent={subagent}"
            );
        }
    }
}

#[test]
fn the_new_rules_cost_exactly_their_lines() {
    // One "- " + text + "\n" per rule line: a rule costs its length plus three bytes.
    // One env for all three: the `<cwd>` line carries the temp path, whose length varies per env.
    let env = TestEnv::new();
    let plain = system(&env, true, false, false);
    let child = system(&env, true, true, false);
    // A child gets the child check instead of the done check (`RULE_VERIFY`, 2026-11-19).
    assert_eq!(
        child.len() + oxa_core::prompt::RULE_VERIFY.len(),
        plain.len() + RULE_CHILD_VERIFY.len()
    );
    let sub_only = system(&env, true, false, true);
    assert_eq!(
        sub_only.len() - plain.len(),
        (RULE_SUBAGENT.len() + 3) + (RULE_SUBAGENT_CHECK.len() + 3),
        "a declared subagent tool adds its rule and the check"
    );
}
