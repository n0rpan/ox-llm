//! Task 60, part B: path containment and the permission modes. The policy is the second half
//! of the sandbox: the file tools ask/deny before they open a path.

use oxa_fake::TestEnv;
use oxa_perm::{ModePolicy, PathGuard};
use oxa_proto::{Access, Decision, Mode, Policy, ToolClass, YoloSwitch};
use std::path::{Path, PathBuf};

fn policy(env: &TestEnv) -> ModePolicy {
    ModePolicy::new().with_guard(PathGuard::new(&env.home(), &env.agent_dir(), &env.tmp()))
}

fn decide(p: &ModePolicy, mode: Mode, a: Access, cwd: &Path) -> Decision {
    p.decide(mode, &a, cwd)
}

fn read(p: PathBuf) -> Access {
    Access::Read(p)
}

fn write(p: PathBuf) -> Access {
    Access::Write(p)
}

#[test]
fn dotdot_is_resolved_before_the_check() {
    let env = TestEnv::new();
    let p = policy(&env);
    let cwd = env.work();
    // `..` out of the project is not inside it.
    assert_eq!(
        decide(&p, Mode::Workspace, write(cwd.join("../outside")), &cwd),
        Decision::Ask
    );
    assert_eq!(
        decide(&p, Mode::Workspace, read(cwd.join("../outside")), &cwd),
        Decision::Ask
    );
    // `..` into a deny-listed path is denied, not merely asked.
    let secret = cwd.join("../home/.ssh/id_test");
    assert_eq!(
        decide(&p, Mode::Workspace, read(secret.clone()), &cwd),
        Decision::Deny(oxa_perm::deny_reasons::SENSITIVE)
    );
    assert_eq!(
        decide(&p, Mode::Workspace, write(secret), &cwd),
        Decision::Deny(oxa_perm::deny_reasons::SENSITIVE)
    );
    // Enough `..` to reach `/` still cannot make a project path look contained.
    assert_eq!(
        decide(
            &p,
            Mode::Workspace,
            write(cwd.join("../../../../etc/hosts")),
            &cwd
        ),
        Decision::Ask
    );
}

#[test]
fn absolute_paths_are_outside_or_denied() {
    let env = TestEnv::new();
    let p = policy(&env);
    let cwd = env.work();
    assert_eq!(
        decide(&p, Mode::Workspace, read(PathBuf::from("/etc/hosts")), &cwd),
        Decision::Ask
    );
    assert_eq!(
        decide(&p, Mode::Workspace, write(PathBuf::from("/etc/hosts")), &cwd),
        Decision::Ask
    );
    assert_eq!(
        decide(&p, Mode::Workspace, read(env.home().join(".netrc")), &cwd),
        Decision::Deny(oxa_perm::deny_reasons::SENSITIVE)
    );
    assert_eq!(
        decide(&p, Mode::Plan, write(PathBuf::from("/etc/hosts")), &cwd),
        Decision::Deny(oxa_perm::deny_reasons::PLAN_WRITE)
    );
}

#[test]
fn symlinks_to_secrets_are_denied() {
    let env = TestEnv::new();
    let cwd = env.work();
    std::fs::create_dir_all(env.home().join(".ssh")).expect("ssh dir");
    std::fs::write(env.home().join(".ssh/id_test"), "SYNTHETIC").expect("key");
    std::os::unix::fs::symlink(env.home().join(".ssh"), cwd.join("link")).expect("symlink");
    let p = policy(&env);
    for a in [
        read(cwd.join("link/id_test")),
        write(cwd.join("link/authorized_keys")),
    ] {
        assert_eq!(
            decide(&p, Mode::Workspace, a, &cwd),
            Decision::Deny(oxa_perm::deny_reasons::SENSITIVE)
        );
    }
    // A symlink to a path outside the project asks, it is not silently inside.
    std::os::unix::fs::symlink("/etc", cwd.join("etc")).expect("symlink");
    assert_eq!(
        decide(&p, Mode::Workspace, read(cwd.join("etc/hosts")), &cwd),
        Decision::Ask
    );
}

#[test]
fn git_metadata_is_never_written() {
    let env = TestEnv::new();
    let p = policy(&env);
    let cwd = env.work();
    // Real files make the containment check independent of how a missing path resolves.
    std::fs::create_dir_all(cwd.join(".git/hooks")).expect("git dir");
    std::fs::write(cwd.join(".git/config"), "[core]\n").expect("config");
    std::fs::write(cwd.join(".git/config.worktree"), "[core]\n").expect("config.worktree");
    for rel in [
        ".git/hooks/pre-commit",
        ".git/config",
        ".git/config.worktree",
        ".git",
    ] {
        assert_eq!(
            decide(&p, Mode::Workspace, write(cwd.join(rel)), &cwd),
            Decision::Deny(oxa_perm::deny_reasons::GIT_PROTECTED),
            "{rel}"
        );
        // `..` and a symlink cannot route around it either.
        assert_eq!(
            decide(&p, Mode::Workspace, write(cwd.join("src/..").join(rel)), &cwd),
            Decision::Deny(oxa_perm::deny_reasons::GIT_PROTECTED),
            ".. {rel}"
        );
    }
}

#[test]
fn project_config_asks_and_plan_denies() {
    let env = TestEnv::new();
    let p = policy(&env);
    let cwd = env.work();
    let cfg = cwd.join(".oxa/settings.json");
    assert_eq!(
        decide(&p, Mode::Workspace, write(cfg.clone()), &cwd),
        Decision::Ask
    );
    assert_eq!(
        decide(&p, Mode::Plan, write(cfg.clone()), &cwd),
        Decision::Deny(oxa_perm::deny_reasons::PLAN_WRITE)
    );
    // The deny-list still wins at `auto` (the old YOLO); `yolo` (full access) removes it
    // outside plan mode (owner's decision on #202), plan mode still refuses.
    let with = |level| {
        ModePolicy::new()
            .with_guard(PathGuard::new(&env.home(), &env.agent_dir(), &env.tmp()))
            .with_yolo(YoloSwitch::new(level))
    };
    let auto = with(oxa_proto::Autonomy::Auto);
    assert_eq!(
        decide(&auto, Mode::Workspace, write(env.home().join(".ssh/id")), &cwd),
        Decision::Deny(oxa_perm::deny_reasons::SENSITIVE)
    );
    assert_eq!(
        decide(&auto, Mode::Workspace, write(cwd.join(".git/config")), &cwd),
        Decision::Deny(oxa_perm::deny_reasons::GIT_PROTECTED)
    );
    let yolo = with(oxa_proto::Autonomy::Yolo);
    for path in [env.home().join(".ssh/id"), cwd.join(".git/config")] {
        assert_eq!(
            decide(&yolo, Mode::Workspace, write(path.clone()), &cwd),
            Decision::Allow
        );
        assert!(matches!(
            decide(&yolo, Mode::Plan, write(path), &cwd),
            Decision::Deny(_)
        ));
    }
}

/// Threat: the user grants a directory, the model replaces that directory with a symlink to a
/// credential, and the grant follows it.
/// Attempt: isolate a directory, remember a write grant, swap the directory for a symlink,
/// then ask again.
/// Expected refusal: the remembered scope is the resolved directory; the swapped path is
/// outside it and the deny-list still denies it.
#[test]
fn a_grant_does_not_follow_a_later_symlink() {
    let env = TestEnv::new();
    let cwd = env.work();
    let dir = cwd.join("granted");
    std::fs::create_dir_all(&dir).expect("dir");
    let mut p = policy(&env);
    p.remember(&write(dir.join("file")));
    assert_eq!(
        decide(&p, Mode::Workspace, write(dir.join("file")), &cwd),
        Decision::Allow
    );
    std::fs::remove_dir_all(&dir).expect("remove");
    std::fs::create_dir_all(env.home().join(".ssh")).expect("ssh");
    std::os::unix::fs::symlink(env.home().join(".ssh"), &dir).expect("symlink");
    assert_eq!(
        decide(&p, Mode::Workspace, write(dir.join("id_test")), &cwd),
        Decision::Deny(oxa_perm::deny_reasons::SENSITIVE)
    );
}

/// Threat: a remote MCP server marks a destructive tool `readOnlyHint` and plan/workspace
/// mode runs it without a prompt. The MCP crate maps an untrusted remote hint to `Write`;
/// this asserts the policy side: `Write` is denied in plan and asks in workspace.
#[test]
fn a_remote_mcp_write_is_not_allowed_in_plan() {
    let env = TestEnv::new();
    let p = policy(&env);
    let cwd = env.work();
    let w = Access::Mcp {
        server: "remote".into(),
        tool: "destroy".into(),
        class: ToolClass::Write,
    };
    assert_eq!(
        decide(&p, Mode::Plan, w.clone(), &cwd),
        Decision::Deny(oxa_perm::deny_reasons::PLAN_MCP)
    );
    assert_eq!(decide(&p, Mode::Workspace, w, &cwd), Decision::Ask);
    let r = Access::Mcp {
        server: "local".into(),
        tool: "look".into(),
        class: ToolClass::Read,
    };
    assert_eq!(decide(&p, Mode::Plan, r.clone(), &cwd), Decision::Allow);
    assert_eq!(decide(&p, Mode::Workspace, r, &cwd), Decision::Allow);
}

#[test]
fn a_control_character_is_a_name_not_a_traversal() {
    let env = TestEnv::new();
    let p = policy(&env);
    let cwd = env.work();
    assert_eq!(
        decide(&p, Mode::Workspace, write(cwd.join("line\nbreak")), &cwd),
        Decision::Allow
    );
    // A newline inside a name is still just part of that one path component.
    assert_eq!(
        decide(&p, Mode::Workspace, write(cwd.join("a\n..").join("b")), &cwd),
        Decision::Allow
    );
}
