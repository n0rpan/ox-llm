//! O-182: YOLO. While the switch is on, every question the policy would ask is answered with
//! "allow" and counted. The safety floor does not move: the deny-list, git's files and plan
//! mode still deny, and a write to the project's `.oxa` configuration still asks.

use oxa_fake::TestEnv;
use oxa_perm::{deny_reasons, ModePolicy, PathGuard};
use oxa_proto::{Access, Decision, Mode, Policy, ToolClass, YoloSwitch};
use std::path::PathBuf;

fn policy(env: &TestEnv, switch: &YoloSwitch) -> ModePolicy {
    ModePolicy::new()
        .with_guard(PathGuard::new(
            &env.home(),
            &env.agent_dir(),
            &env.agent_dir().join("tmp"),
        ))
        .with_yolo(switch.clone())
}

/// One access of every kind that the table answers with `Ask` in `workspace` mode.
fn asked_in_workspace(env: &TestEnv) -> Vec<Access> {
    vec![
        Access::Read(env.root().join("elsewhere/notes.txt")),
        Access::Write(env.root().join("elsewhere/out.txt")),
        Access::Exec {
            command: "curl https://example.invalid".to_string(),
            net: true,
        },
        Access::Net {
            host: "example.invalid".to_string(),
        },
        Access::Mcp {
            server: "srv".to_string(),
            tool: "change".to_string(),
            class: ToolClass::Write,
        },
        Access::Mcp {
            server: "srv".to_string(),
            tool: "run".to_string(),
            class: ToolClass::Exec,
        },
    ]
}

#[test]
fn off_asks_and_on_allows_every_kind_and_counts() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(false);
    let p = policy(&env, &switch);
    let cwd = env.work();
    let accesses = asked_in_workspace(&env);
    for a in &accesses {
        assert_eq!(
            p.decide(Mode::Workspace, a, &cwd),
            Decision::Ask,
            "{a:?} asks without YOLO"
        );
    }
    assert_eq!(switch.approved(), 0, "nothing is counted while YOLO is off");

    switch.set(true);
    for a in &accesses {
        assert_eq!(
            p.decide(Mode::Workspace, a, &cwd),
            Decision::Allow,
            "{a:?} under YOLO"
        );
    }
    assert_eq!(switch.approved(), accesses.len() as u64);

    // `ask` mode asks for everything, also inside the work tree: all of it is auto-approved.
    let inside = [
        Access::Read(cwd.join("a.txt")),
        Access::Write(cwd.join("a.txt")),
        Access::Exec {
            command: "ls".to_string(),
            net: false,
        },
        Access::Mcp {
            server: "srv".to_string(),
            tool: "look".to_string(),
            class: ToolClass::Read,
        },
    ];
    for a in inside.iter().chain(&accesses) {
        assert_eq!(
            p.decide(Mode::Ask, a, &cwd),
            Decision::Allow,
            "{a:?} in ask mode under YOLO"
        );
    }

    // Off again: the prompts are back, and the count stays.
    let counted = switch.approved();
    switch.set(false);
    for a in &accesses {
        assert_eq!(p.decide(Mode::Workspace, a, &cwd), Decision::Ask);
    }
    assert_eq!(switch.approved(), counted);
}

#[test]
fn what_needed_no_prompt_is_not_counted() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(true);
    let p = policy(&env, &switch);
    let cwd = env.work();
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Write(cwd.join("a.txt")), &cwd),
        Decision::Allow
    );
    assert_eq!(switch.approved(), 0);
}

#[test]
fn the_deny_list_stands_under_yolo() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(true);
    let p = policy(&env, &switch);
    let cwd = env.work();
    let sensitive = Decision::Deny(deny_reasons::SENSITIVE);
    let home = env.home();
    let paths: Vec<PathBuf> = vec![
        home.join(".ssh/authorized_keys"),
        home.join(".ssh/id_ed25519"),
        home.join(".gnupg/private-keys-v1.d/key"),
        home.join(".config/oxc/token"),
        home.join(".aws/credentials"),
        home.join(".netrc"),
        env.agent_dir().join("settings.json"),
    ];
    for mode in Mode::ALL {
        for path in &paths {
            assert_eq!(
                p.decide(mode, &Access::Write(path.clone()), &cwd),
                sensitive,
                "write {path:?} in {mode:?} under YOLO"
            );
            assert_eq!(
                p.decide(mode, &Access::Read(path.clone()), &cwd),
                sensitive,
                "read {path:?} in {mode:?} under YOLO"
            );
        }
    }
    assert_eq!(
        switch.approved(),
        0,
        "a denied access is never counted as approved"
    );
}

#[test]
fn git_files_plan_mode_and_project_config_keep_their_answers() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(true);
    let p = policy(&env, &switch);
    let cwd = env.work();
    // Git's own files run outside the sandbox: never written by a tool.
    for rel in [".git/hooks/pre-commit", ".git/config"] {
        assert_eq!(
            p.decide(Mode::Workspace, &Access::Write(cwd.join(rel)), &cwd),
            Decision::Deny(deny_reasons::GIT_PROTECTED)
        );
    }
    // The project's own configuration is loaded outside the sandbox later: it still asks.
    assert_eq!(
        p.decide(
            Mode::Workspace,
            &Access::Write(cwd.join(".oxa/settings.json")),
            &cwd
        ),
        Decision::Ask,
        "YOLO does not auto-approve writes to the project configuration"
    );
    // Plan mode stays read-only.
    assert_eq!(
        p.decide(Mode::Plan, &Access::Write(cwd.join("a.txt")), &cwd),
        Decision::Deny(deny_reasons::PLAN_WRITE)
    );
    assert_eq!(
        p.decide(
            Mode::Plan,
            &Access::Exec {
                command: "rm -rf x".to_string(),
                net: false
            },
            &cwd
        ),
        Decision::Deny(deny_reasons::PLAN_EXEC)
    );
    assert_eq!(switch.approved(), 0);
}

#[test]
fn clones_share_one_switch() {
    let a = YoloSwitch::new(false);
    let b = a.clone();
    b.set(true);
    b.count();
    assert!(a.is_on() && a.approved() == 1 && a.same(&b));
    assert!(!a.same(&YoloSwitch::new(true)));
}

/// O-182, subagents: a child started with `--yolo` builds its policy from `YoloSwitch::new(true)`
/// (`dispatch.rs`), the same as the parent with the flag. It allows an ordinary write without
/// asking, and the floor of the parent holds for it: a write to the project's `.oxa` asks, the
/// deny-list denies.
#[test]
fn a_child_started_with_yolo_allows_a_write_and_still_asks_for_oxa() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(true);
    let p = policy(&env, &switch);
    let cwd = env.work();
    let outside = env.root().join("elsewhere/out.txt");
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Write(outside), &cwd),
        Decision::Allow,
        "an ordinary write is allowed without a prompt"
    );
    assert_eq!(switch.approved(), 1);
    for rel in [".oxa/settings.json", ".oxa/agents/scout.md"] {
        assert_eq!(
            p.decide(Mode::Workspace, &Access::Write(cwd.join(rel)), &cwd),
            Decision::Ask,
            "{rel} still asks under YOLO"
        );
    }
    assert_eq!(
        p.decide(
            Mode::Workspace,
            &Access::Write(env.home().join(".ssh/authorized_keys")),
            &cwd
        ),
        Decision::Deny(deny_reasons::SENSITIVE)
    );
    assert_eq!(switch.approved(), 1, "the asked and the denied are not counted");
}
