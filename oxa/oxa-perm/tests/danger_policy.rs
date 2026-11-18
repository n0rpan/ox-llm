//! #1512 S2: dangerous commands are a floor prompt in the policy. The classifier itself is
//! tested in `danger_table.rs`; these tests pin what the policy does with its hits.
use oxa_perm::{DangerRules, ModePolicy, PathGuard};
use oxa_proto::{Access, Autonomy, Decision, Mode, Policy, YoloSwitch};
use std::path::Path;
use std::sync::Arc;

const CWD: &str = "/work/project";
const DANGEROUS: &str = "git push --force origin main";

fn cwd() -> &'static Path {
    Path::new(CWD)
}

fn guard() -> PathGuard {
    PathGuard::new(
        Path::new("/home/u"),
        Path::new("/home/u/.oxa/agent"),
        Path::new("/tmp/oxa-test"),
    )
}

fn exec(command: &str) -> Access {
    Access::Exec {
        command: command.to_string(),
        net: false,
    }
}

/// A policy with the built-in rules and the given always keys, at the given autonomy.
fn policy(always: Vec<String>, level: Autonomy) -> ModePolicy {
    ModePolicy::new()
        .with_guard(guard())
        .with_yolo(YoloSwitch::new(level))
        .with_danger(Arc::new(DangerRules::builtin()), always)
}

/// The grant keys the policy reports for `command` (what a session or always answer stores).
fn keys_of(command: &str) -> Vec<String> {
    policy(Vec::new(), Autonomy::Standard)
        .danger_ask(&exec(command), cwd())
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

#[test]
fn dangerous_exec_asks_in_standard() {
    let p = policy(Vec::new(), Autonomy::Standard);
    assert_eq!(p.decide(Mode::Workspace, &exec(DANGEROUS), cwd()), Decision::Ask);
}

#[test]
fn dangerous_exec_asks_in_auto_and_is_not_counted() {
    let switch = YoloSwitch::new(Autonomy::Auto);
    let p = ModePolicy::new()
        .with_guard(guard())
        .with_yolo(switch.clone())
        .with_danger(Arc::new(DangerRules::builtin()), Vec::new());
    assert_eq!(p.decide(Mode::Workspace, &exec(DANGEROUS), cwd()), Decision::Ask);
    assert_eq!(
        switch.approved(),
        0,
        "a floor prompt is a question that was not answered by auto"
    );
}

#[test]
fn dangerous_exec_allowed_and_counted_in_yolo() {
    let switch = YoloSwitch::new(Autonomy::Yolo);
    let p = ModePolicy::new()
        .with_guard(guard())
        .with_yolo(switch.clone())
        .with_danger(Arc::new(DangerRules::builtin()), Vec::new());
    assert_eq!(
        p.decide(Mode::Workspace, &exec(DANGEROUS), cwd()),
        Decision::Allow
    );
    assert_eq!(switch.approved(), 1);
}

#[test]
fn dangerous_exec_denied_in_plan() {
    let switch = YoloSwitch::new(Autonomy::Yolo);
    let p = ModePolicy::new()
        .with_guard(guard())
        .with_yolo(switch)
        .with_danger(Arc::new(DangerRules::builtin()), Vec::new());
    assert!(matches!(
        p.decide(Mode::Plan, &exec(DANGEROUS), cwd()),
        Decision::Deny(_)
    ));
}

#[test]
fn safe_exec_still_allowed_without_prompt_in_workspace() {
    let p = policy(Vec::new(), Autonomy::Standard);
    assert_eq!(
        p.decide(Mode::Workspace, &exec("cargo build"), cwd()),
        Decision::Allow
    );
    assert!(p.danger_ask(&exec("cargo build"), cwd()).is_empty());
}

#[test]
fn generic_exec_session_grant_does_not_cover_dangerous() {
    let mut p = policy(Vec::new(), Autonomy::Standard);
    // The generic grant of "any command without network" (an `Exec { net: false }` remember).
    p.remember(&exec("cargo build"));
    assert_eq!(p.decide(Mode::Workspace, &exec(DANGEROUS), cwd()), Decision::Ask);
    assert_eq!(
        p.decide(Mode::Workspace, &exec("cargo test"), cwd()),
        Decision::Allow
    );
}

#[test]
fn session_danger_grant_covers_exact_key_only() {
    let mut p = policy(Vec::new(), Autonomy::Standard);
    let keys = keys_of(DANGEROUS);
    assert!(!keys.is_empty());
    p.grant_danger(&keys);
    assert_eq!(
        p.decide(Mode::Workspace, &exec(DANGEROUS), cwd()),
        Decision::Allow
    );
    // Another force push is another segment, so another key.
    assert_eq!(
        p.decide(Mode::Workspace, &exec("git push --force origin dev"), cwd()),
        Decision::Ask
    );
    // Granting twice keeps one key.
    p.grant_danger(&keys);
    assert_eq!(
        p.decide(Mode::Workspace, &exec(DANGEROUS), cwd()),
        Decision::Allow
    );
}

#[test]
fn always_list_from_user_settings_allows_exact_key() {
    let keys = keys_of(DANGEROUS);
    let p = policy(keys, Autonomy::Standard);
    assert_eq!(
        p.decide(Mode::Workspace, &exec(DANGEROUS), cwd()),
        Decision::Allow
    );
    assert_eq!(
        p.decide(Mode::Workspace, &exec("git push --force origin dev"), cwd()),
        Decision::Ask
    );
}

#[test]
fn second_hit_in_same_line_still_asks_when_first_granted() {
    let line = "git push --force origin main && sudo ls";
    let hits = policy(Vec::new(), Autonomy::Standard).danger_ask(&exec(line), cwd());
    assert!(hits.len() >= 2, "two rules must match: {hits:?}");
    let mut p = policy(Vec::new(), Autonomy::Standard);
    p.grant_danger(&[hits[0].0.clone()]);
    assert_eq!(p.decide(Mode::Workspace, &exec(line), cwd()), Decision::Ask);
    let all: Vec<String> = hits.into_iter().map(|(key, _)| key).collect();
    p.grant_danger(&all);
    assert_eq!(p.decide(Mode::Workspace, &exec(line), cwd()), Decision::Allow);
}

#[test]
fn danger_policies_compare_their_grants() {
    let a = policy(Vec::new(), Autonomy::Standard);
    let mut b = a.clone();
    assert_eq!(a, b);
    b.grant_danger(&keys_of(DANGEROUS));
    assert_ne!(a, b, "a danger grant is part of the policy's state");
}

/// A grant never covers a command the shell computes at run time: `rm -rf "$D"` granted for one
/// value of `D` would otherwise delete whatever `D` names next time, in every later session for
/// an always key.
#[test]
fn dynamic_segment_grant_never_covers_another_value() {
    let line = r#"rm -rf "$D""#;
    let keys = keys_of(line);
    assert!(
        !keys.is_empty(),
        "a variable target is outside until proven inside"
    );
    let mut p = policy(keys.clone(), Autonomy::Standard);
    p.grant_danger(&keys);
    assert_eq!(p.decide(Mode::Workspace, &exec(line), cwd()), Decision::Ask);
    assert_eq!(
        p.decide(Mode::Workspace, &exec(r#"D=$HOME; rm -rf "$D""#), cwd()),
        Decision::Ask
    );
    assert!(!p.danger_ask(&exec(line), cwd()).is_empty());
}
