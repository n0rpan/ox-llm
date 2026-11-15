//! O-182, #202: `auto` (the old YOLO) and `yolo` (full access). At `auto` every question the
//! policy would ask is answered with "allow" and counted, and the safety floor does not move: the
//! deny-list, git's files and plan mode still deny, a write to `.oxa` still asks. At `yolo` the
//! sandbox of bash is lifted and the file tools keep no guard either (plan mode excepted).

use oxa_fake::TestEnv;
use oxa_perm::{deny_reasons, ModePolicy, PathGuard};
use oxa_proto::{Access, Autonomy, Decision, Mode, Policy, ToolClass, YoloSwitch};
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
    let switch = YoloSwitch::new(Autonomy::Standard);
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

    switch.set_level(Autonomy::Auto);
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
    switch.set_level(Autonomy::Standard);
    for a in &accesses {
        assert_eq!(p.decide(Mode::Workspace, a, &cwd), Decision::Ask);
    }
    assert_eq!(switch.approved(), counted);
}

#[test]
fn what_needed_no_prompt_is_not_counted() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(Autonomy::Auto);
    let p = policy(&env, &switch);
    let cwd = env.work();
    assert_eq!(
        p.decide(Mode::Workspace, &Access::Write(cwd.join("a.txt")), &cwd),
        Decision::Allow
    );
    assert_eq!(switch.approved(), 0);
}

#[test]
fn the_deny_list_stands_under_auto() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(Autonomy::Auto);
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
                "write {path:?} in {mode:?} under auto"
            );
            assert_eq!(
                p.decide(mode, &Access::Read(path.clone()), &cwd),
                sensitive,
                "read {path:?} in {mode:?} under auto"
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
    let switch = YoloSwitch::new(Autonomy::Auto);
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
        "auto does not auto-approve writes to the project configuration"
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
    let a = YoloSwitch::new(Autonomy::Standard);
    let b = a.clone();
    b.set_level(Autonomy::Auto);
    b.count();
    assert!(a.is_on() && a.approved() == 1 && a.same(&b));
    assert!(!a.same(&YoloSwitch::new(Autonomy::Auto)));
}

/// O-182, subagents: a child started with `--auto` builds its policy from `YoloSwitch::new(Autonomy::Auto)`
/// (`dispatch.rs`), the same as the parent with the flag. It allows an ordinary write without
/// asking, and the floor of the parent holds for it: a write to the project's `.oxa` asks, the
/// deny-list denies.
#[test]
fn a_child_started_with_auto_allows_a_write_and_still_asks_for_oxa() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(Autonomy::Auto);
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
            "{rel} still asks under auto"
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

/// #202, owner's decision on #314 ("yolo should be yolo"): outside plan mode `yolo` removes
/// every guard of the file tools: the table's questions, the deny-list, git's files and the
/// protected configuration (`.oxa`, the home's startup files) are all allowed and counted.
#[test]
fn yolo_keeps_the_hard_floor_and_asks_nothing() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(Autonomy::Yolo);
    let p = policy(&env, &switch);
    let cwd = env.work();
    let home = env.home();
    for mode in [Mode::Workspace, Mode::Ask] {
        // Review of #314: a table Ask turned into Allow is counted once, not by both the
        // floor's `auto` answer and the yolo branch.
        for a in &asked_in_workspace(&env) {
            let before = switch.approved();
            assert_eq!(p.decide(mode, a, &cwd), Decision::Allow, "{a:?} in {mode:?}");
            assert_eq!(switch.approved(), before + 1, "{a:?} is counted once");
        }
        // The hard floor stands (review of #314): credentials and git hooks/config are refused,
        // not counted.
        for a in [
            Access::Read(home.join(".ssh/id_ed25519")),
            Access::Write(home.join(".aws/credentials")),
            Access::Write(cwd.join(".git/hooks/x")),
            Access::Write(cwd.join(".git/config")),
        ] {
            let before = switch.approved();
            assert!(
                matches!(p.decide(mode, &a, &cwd), Decision::Deny(_)),
                "{a:?} in {mode:?}"
            );
            assert_eq!(switch.approved(), before, "{a:?} is not counted");
        }
        // What only asked (project config, the home's startup files) is allowed and counted.
        for a in [
            Access::Write(cwd.join(".oxa/settings.json")),
            Access::Write(home.join(".bashrc")),
        ] {
            let before = switch.approved();
            assert_eq!(p.decide(mode, &a, &cwd), Decision::Allow, "{a:?} in {mode:?}");
            assert_eq!(switch.approved(), before + 1, "{a:?} is counted once");
        }
    }
    // An ordinary write inside the work tree needed no answer: not counted.
    let before = switch.approved();
    p.decide(Mode::Workspace, &Access::Write(cwd.join("a.txt")), &cwd);
    assert_eq!(switch.approved(), before);
}

/// #202: the same three guarded writes keep their answers in `auto`.
#[test]
fn auto_keeps_every_guard() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(Autonomy::Auto);
    let p = policy(&env, &switch);
    let cwd = env.work();
    let w = |path: PathBuf| p.decide(Mode::Workspace, &Access::Write(path), &cwd);
    assert_eq!(w(cwd.join(".oxa/settings.json")), Decision::Ask);
    assert_eq!(
        w(cwd.join(".git/hooks/x")),
        Decision::Deny(deny_reasons::GIT_PROTECTED)
    );
    assert_eq!(
        w(env.home().join(".ssh/id_ed25519")),
        Decision::Deny(deny_reasons::SENSITIVE)
    );
}

/// #202: plan mode still plans only under `yolo`: writes and changing commands are refused,
/// and the deny-list stands there.
#[test]
fn plan_mode_still_refuses_writes_under_yolo() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(Autonomy::Yolo);
    let p = policy(&env, &switch);
    let cwd = env.work();
    assert_eq!(
        p.decide(Mode::Plan, &Access::Write(cwd.join("a.txt")), &cwd),
        Decision::Deny(deny_reasons::PLAN_WRITE)
    );
    assert_eq!(
        p.decide(Mode::Plan, &Access::Write(env.root().join("elsewhere/x")), &cwd),
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
    assert_eq!(
        p.decide(
            Mode::Plan,
            &Access::Read(env.home().join(".ssh/id_ed25519")),
            &cwd
        ),
        Decision::Deny(deny_reasons::SENSITIVE)
    );
}

/// #202: switching from `yolo` back to `auto` brings the floor back at once.
#[test]
fn auto_after_yolo_restores_the_floor() {
    let env = TestEnv::new();
    let switch = YoloSwitch::new(Autonomy::Yolo);
    let p = policy(&env, &switch);
    let cwd = env.work();
    let out = Access::Read(env.root().join("elsewhere/notes.txt"));
    assert_eq!(p.decide(Mode::Workspace, &out, &cwd), Decision::Allow);
    switch.set_level(Autonomy::Auto);
    let key = Access::Read(env.home().join(".ssh/id_ed25519"));
    assert_eq!(
        p.decide(Mode::Workspace, &key, &cwd),
        Decision::Deny(deny_reasons::SENSITIVE)
    );
    assert_eq!(
        p.decide(
            Mode::Workspace,
            &Access::Write(cwd.join(".oxa/settings.json")),
            &cwd
        ),
        Decision::Ask
    );
}

/// Review of #314: the one-line description shown in the picker, the start notice and stderr
/// must say what the policy does at `yolo`: every guard is gone (pinned by the tests above).
#[test]
fn yolo_description_says_every_guard_is_gone() {
    use oxa_proto::PermissionMode;
    let text = PermissionMode::Yolo.describe();
    assert!(text.contains("without the sandbox"), "{text}");
    assert!(!text.contains("not limited"), "{text}");
    assert!(!text.contains("no deny-list"), "{text}");
    assert!(text.contains("deny-list"), "{text}");
}
