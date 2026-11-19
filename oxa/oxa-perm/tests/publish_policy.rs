//! Owner decision 2026-11-19: a workspace worker publishes its own branch without asking; every
//! other publish, and every command chained with one, asks.
use oxa_perm::{DangerRules, ModePolicy, PathGuard};
use oxa_proto::{Access, Autonomy, Decision, Mode, Policy, YoloSwitch};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn repo(branch: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oxa-publish-{}-{}",
        std::process::id(),
        branch.replace('/', "_")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join(".git/HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    dir
}

fn decide(cwd: &Path, level: Autonomy, command: &str) -> Decision {
    let guard = PathGuard::new(
        Path::new("/home/u"),
        Path::new("/home/u/.oxa/agent"),
        Path::new("/tmp/oxa-test"),
    );
    let p = ModePolicy::new()
        .with_guard(guard)
        .with_yolo(YoloSwitch::new(level))
        .with_danger(Arc::new(DangerRules::builtin()), Vec::new());
    p.decide(
        Mode::Workspace,
        &Access::Exec {
            command: command.into(),
            net: false,
        },
        cwd,
    )
}

#[test]
fn own_branch_publish_is_allowed() {
    let cwd = repo("my-branch");
    for c in [
        "git push",
        "git push origin HEAD",
        "git push -u origin my-branch",
        "git push origin HEAD:my-branch",
        "GIT_TERMINAL_PROMPT=0 git push",
        "git -C . push origin HEAD",
        "git status && git push",
        "git push && gh pr create --title t --body b",
        "gh pr checks",
        "gh pr view --json state",
        "gh run watch 12 --exit-status",
        "gh api repos/o/r/pulls/1/comments",
        "git push 2>&1 | tail -n 5",
    ] {
        for level in [Autonomy::Standard, Autonomy::Auto] {
            assert_eq!(decide(&cwd, level, c), Decision::Allow, "{c}");
        }
    }
}

#[test]
fn everything_else_asks() {
    let cwd = repo("my-branch");
    for c in [
        "curl x && git push",
        "git push && curl x",
        "git push; rm -rf y",
        "git push | tee f",
        "git push $(curl x)",
        "git push `curl x`",
        "bash -c \"git push && curl x\"",
        "sh -c 'git push'",
        "eval \"git push\"",
        "git push origin +my-branch",
        "git push origin HEAD:main",
        "git push origin HEAD:refs/heads/main",
        "git push --mirror",
        "git push --all",
        "git push --tags",
        "git push --force",
        "git push origin :my-branch",
        "git push upstream my-branch",
        "git -c remote.origin.url=https://evil/r push",
        "git push https://other/repo",
        "gh pr merge 5 --squash",
        "gh pr create -R other/repo",
        "gh release create v1",
        "gh api -X POST repos/o/r/issues",
        "gh api repos/o/r/issues -f title=x",
        "GIT_SSH_COMMAND=evil git push",
    ] {
        for level in [Autonomy::Standard, Autonomy::Auto] {
            assert_eq!(decide(&cwd, level, c), Decision::Ask, "{c}");
        }
        // Yolo is unchanged: it never asks.
        assert_eq!(decide(&cwd, Autonomy::Yolo, c), Decision::Allow, "yolo {c}");
    }
}

#[test]
fn default_branch_push_asks_even_when_checked_out() {
    for b in ["main", "master"] {
        let cwd = repo(b);
        for c in ["git push", "git push origin HEAD", "git push origin main"] {
            assert_eq!(decide(&cwd, Autonomy::Standard, c), Decision::Ask, "{b}: {c}");
        }
    }
}
