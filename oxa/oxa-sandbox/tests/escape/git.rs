//! Git: hooks and config run outside the sandbox later, so a command must not write them,
//! whatever layout the repository uses for its git directory.

use crate::harness::{assert_ok, assert_refused, dir, run, sandbox, sandbox_at, write};
use oxa_fake::TestEnv;
use std::path::Path;

fn config(work: &Path, body: &str) {
    dir(&work.join(".git/hooks"));
    dir(&work.join(".git/objects"));
    write(&work.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(&work.join(".git/config"), body);
}

fn plain(env: &TestEnv) -> std::path::PathBuf {
    let work = env.work();
    config(&work, "[core]\n\trepositoryformatversion = 0\n");
    work
}

/// Threat: a command writes `.git/hooks/pre-commit` or `.git/config`, which git runs/reads
/// outside the sandbox on the user's next command.
/// Attempt: append the config and plant a hook, with the entries both present and missing.
/// Expected refusal: both are read-only; a missing one cannot be created either.
#[test]
fn hooks_and_config_are_read_only() {
    let env = TestEnv::new();
    let work = plain(&env);
    let Some(sb) = sandbox(&env) else { return };
    for cmd in [
        "printf '\\n[core]\\n\\thooksPath = .\\n' >> .git/config",
        "printf '#!/bin/sh\\n' > .git/hooks/pre-commit",
    ] {
        let (exit, out) = run(&sb, &work, cmd, true, false);
        assert_refused(exit, &out, cmd);
    }
    assert_eq!(
        std::fs::read_to_string(work.join(".git/config")).expect("config"),
        "[core]\n\trepositoryformatversion = 0\n"
    );
    assert!(!work.join(".git/hooks/pre-commit").exists());

    // Missing entries: the sandbox masks/creates an empty read-only file or directory.
    let env = TestEnv::new();
    let work = env.work();
    dir(&work.join(".git/objects"));
    write(&work.join(".git/HEAD"), "ref: refs/heads/main\n");
    let Some(sb) = sandbox(&env) else { return };
    for cmd in [
        "printf '#!/bin/sh\\n' > .git/hooks/pre-commit",
        "printf '[core]\\n\\thooksPath = .\\n' > .git/config",
    ] {
        let (exit, out) = run(&sb, &work, cmd, true, false);
        assert_refused(exit, &out, cmd);
    }
    assert!(!work.join(".git/hooks/pre-commit").exists());
    let cfg = std::fs::read_to_string(work.join(".git/config")).unwrap_or_default();
    assert!(!cfg.contains("hooksPath"), "config created: {cfg}");
}

/// Threat: a repository config includes another file and hides `core.hooksPath` there.
/// Attempt: `[include] path = hooks.cfg` with `hooksPath = .githooks`, then write `.githooks`.
/// Expected refusal: the included value is followed and its directory is read-only too.
#[test]
fn a_hooks_path_in_an_included_config_is_protected() {
    let env = TestEnv::new();
    let work = env.work();
    config(
        &work,
        "[core]\n\trepositoryformatversion = 0\n[include]\n\tpath = hooks.cfg\n",
    );
    write(&work.join(".git/hooks.cfg"), "[core]\n\thooksPath = .githooks\n");
    dir(&work.join(".githooks"));
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(
        &sb,
        &work,
        "printf '#!/bin/sh\\n' > .githooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "included hooksPath");
    assert!(!work.join(".githooks/pre-commit").exists());
}

/// Threat: `core.hooksPath` names a project directory; `.git/hooks` is protected but that
/// directory is not.
/// Attempt: write a hook into `.githooks`, both when it exists and when it does not.
/// Expected refusal: the configured directory is read-only (or masked when missing).
#[test]
fn the_configured_hooks_path_is_read_only() {
    let env = TestEnv::new();
    let work = env.work();
    config(
        &work,
        "[core]\n\trepositoryformatversion = 0\n\thooksPath = .githooks\n",
    );
    dir(&work.join(".githooks"));
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(
        &sb,
        &work,
        "printf '#!/bin/sh\\n' > .githooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "existing hooksPath");
    assert!(!work.join(".githooks/pre-commit").exists());

    let env = TestEnv::new();
    let work = env.work();
    config(
        &work,
        "[core]\n\trepositoryformatversion = 0\n\thooksPath = .githooks\n",
    );
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(
        &sb,
        &work,
        "mkdir -p .githooks && printf '#!/bin/sh\\n' > .githooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "missing hooksPath");
    assert!(!work.join(".githooks/pre-commit").exists());
}

/// Threat: `.git` is a symlink to a git directory inside the project; renaming the target and
/// putting a new one in its place leaves the hooks writable while git still follows the link.
/// Attempt: write `.git/hooks/pre-commit` (and the target config) through the symlink.
/// Expected refusal: the sandbox refuses a symlinked `.git` instead of guessing at its target.
#[test]
fn a_symlinked_dotgit_is_refused() {
    let env = TestEnv::new();
    let work = env.work();
    dir(&work.join("gitdata/hooks"));
    dir(&work.join("gitdata/objects"));
    write(&work.join("gitdata/HEAD"), "ref: refs/heads/main\n");
    write(&work.join("gitdata/config"), "[core]\n");
    std::os::unix::fs::symlink("gitdata", work.join(".git")).expect("symlink");
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(
        &sb,
        &work,
        "mv gitdata gitdata.old && mkdir -p gitdata/hooks && printf '#!/bin/sh\\n' > gitdata/hooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "symlinked .git");
    assert!(out.contains("sandbox refused"), "{out}");
    assert!(!work.join("gitdata/hooks/pre-commit").exists());
}

/// Threat: a worktree `.git` file names a git directory inside the project; the file is
/// read-only but the gitdir it names is part of the writable cwd.
/// Attempt: write `gitdata/hooks/pre-commit` and `gitdata/config` behind the `.git` file.
/// Expected refusal: a `.git` file that names a writable directory is refused.
#[test]
fn a_worktree_gitdir_inside_the_project_is_refused() {
    let env = TestEnv::new();
    let work = env.work();
    dir(&work.join("gitdata/hooks"));
    dir(&work.join("gitdata/objects"));
    write(&work.join("gitdata/HEAD"), "ref: refs/heads/main\n");
    write(
        &work.join(".git"),
        &format!("gitdir: {}\n", work.join("gitdata").display()),
    );
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(
        &sb,
        &work,
        "mkdir -p gitdata/hooks && printf '#!/bin/sh\\n' > gitdata/hooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "worktree gitdir inside cwd");
    assert!(out.contains("sandbox refused"), "{out}");
    assert!(!work.join("gitdata/hooks/pre-commit").exists());
}

/// Threat: an `includeIf` in `.git/config` points at another config that sets a hook path.
/// Attempt: append to `.git/config` and write the include target.
/// Expected refusal: a config with includes makes the git directory read-only for the run.
#[test]
fn include_if_config_is_not_writable() {
    let env = TestEnv::new();
    let work = env.work();
    let inc = env.tmp().join("hooks.cfg");
    write(
        &work.join(".git/config"),
        &format!(
            "[core]\n\trepositoryformatversion = 0\n[includeIf \"gitdir:{}/\"]\n\tpath = {}\n",
            work.display(),
            inc.display()
        ),
    );
    dir(&work.join(".git/hooks"));
    dir(&work.join(".git/objects"));
    write(&work.join(".git/HEAD"), "ref: refs/heads/main\n");
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(&sb, &work, "printf x >> .git/config", true, false);
    assert_refused(exit, &out, "includeIf config");
    assert_eq!(
        std::fs::read_to_string(work.join(".git/config")).expect("config"),
        format!(
            "[core]\n\trepositoryformatversion = 0\n[includeIf \"gitdir:{}/\"]\n\tpath = {}\n",
            work.display(),
            inc.display()
        )
    );
}

/// Threat: the session builds its layout from a safe repository, then the project replaces
/// `.git` with a symlink before the next command (a check/use race).
/// Attempt: build the sandbox, swap `.git` for a symlink to a writable git directory, then run.
/// Expected refusal: the per-command check sees the symlink and refuses; nothing is written.
#[test]
fn a_git_swap_after_start_is_rechecked() {
    let env = TestEnv::new();
    let work = plain(&env);
    let Some(sb) = sandbox(&env) else { return };
    std::fs::rename(work.join(".git"), work.join("gitdata")).expect("rename .git");
    dir(&work.join("gitdata/hooks"));
    write(&work.join("gitdata/config"), "[core]\n");
    std::os::unix::fs::symlink("gitdata", work.join(".git")).expect("symlink");
    let (exit, out) = run(
        &sb,
        &work,
        "printf '#!/bin/sh\\n' > gitdata/hooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "swapped .git");
    assert!(out.contains("sandbox refused"), "{out}");
    assert!(!work.join("gitdata/hooks/pre-commit").exists());
}

/// Threat: a `.git` file names a not-yet-existing gitdir with `..` in the path, so a lexical
/// check misses it and the command creates the directory and a hook.
/// Attempt: `.git` -> `../work/gitdata` (missing), then create `gitdata/hooks/pre-commit`.
/// Expected refusal: the target is normalised to the writable cwd and refused.
#[test]
fn a_dotgit_file_with_dotdot_to_a_missing_gitdir_is_refused() {
    let env = TestEnv::new();
    let work = env.work();
    let name = work.file_name().expect("cwd name");
    write(
        &work.join(".git"),
        &format!("gitdir: ../{}/gitdata\n", name.to_string_lossy()),
    );
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(
        &sb,
        &work,
        "mkdir -p gitdata/hooks && printf '#!/bin/sh\\n' > gitdata/hooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "dotdot .git target");
    assert!(out.contains("sandbox refused"), "{out}");
    assert!(!work.join("gitdata/hooks/pre-commit").exists());
}

/// Threat: a real `.git` directory has a `commondir` file pointing at a writable directory in
/// the project; git reads hooks and config from the common dir, not from `.git`.
/// Attempt: `.git/commondir` -> `../gitdata`, then write `gitdata/hooks/pre-commit`.
/// Expected refusal: the common dir lies in what the command can write, so commands are refused.
#[test]
fn a_gitdir_with_a_commondir_inside_the_project_is_refused() {
    let env = TestEnv::new();
    let work = env.work();
    dir(&work.join(".git/hooks"));
    dir(&work.join(".git/objects"));
    write(&work.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(&work.join(".git/config"), "[core]\n");
    write(&work.join(".git/commondir"), "../gitdata\n");
    dir(&work.join("gitdata/hooks"));
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(
        &sb,
        &work,
        "printf '#!/bin/sh\\n' > gitdata/hooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "commondir inside cwd");
    assert!(out.contains("sandbox refused"), "{out}");
    assert!(!work.join("gitdata/hooks/pre-commit").exists());
}

/// Threat: `core.hooksPath` is written with `..` so a lexical check does not see that it
/// points back inside the project.
/// Attempt: `hooksPath = ../<cwd>/gitdata/hooks`, then write a hook there.
/// Expected refusal: the path is normalised before the containment check and protected.
#[test]
fn a_hooks_path_with_dotdot_is_normalised() {
    let env = TestEnv::new();
    let work = env.work();
    let name = work.file_name().expect("cwd name").to_string_lossy().into_owned();
    config(
        &work,
        &format!("[core]\n\trepositoryformatversion = 0\n\thooksPath = ../{name}/gitdata/hooks\n"),
    );
    dir(&work.join("gitdata/hooks"));
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(
        &sb,
        &work,
        "printf '#!/bin/sh\\n' > gitdata/hooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "dotdot hooksPath");
    assert!(!work.join("gitdata/hooks/pre-commit").exists());
}

/// Threat: `core.hooksPath` points into a writable package cache, and plan mode still binds
/// caches read-write, so a read-only command can plant a hook there.
/// Attempt: plan mode, `hooksPath` in a layout write bind, write the hook.
/// Expected refusal: the configured hooks path is read-only in plan mode too.
#[test]
fn a_hooks_path_in_a_writable_cache_is_read_only_in_plan_mode() {
    let env = TestEnv::new();
    let work = env.work();
    write(&env.home().join(".cache/pip/hooks/keep"), "keep\n");
    config(
        &work,
        "[core]\n\trepositoryformatversion = 0\n\thooksPath = ~/.cache/pip/hooks\n",
    );
    let mut layout = oxa_sandbox::Layout::default();
    layout.write.push(env.home().join(".cache/pip"));
    let Some(sb) = crate::harness::sandbox_with_layout(&env, layout) else {
        return;
    };
    let (exit, out) = run(
        &sb,
        &work,
        "printf '#!/bin/sh\\n' > ~/.cache/pip/hooks/pre-commit",
        false,
        false,
    );
    assert_refused(exit, &out, "hooksPath in a writable cache, plan mode");
    assert!(!env.home().join(".cache/pip/hooks/pre-commit").exists());
}

/// Threat: a `.git` file points at an external git directory the sandbox did not validate;
/// its config can still name `.githooks` in this writable cwd.
/// Attempt: `.git` -> an external directory that does not pass the worktree checks, then write
/// `.githooks/pre-commit`.
/// Expected refusal: an unvalidated `.git` file is refused.
#[test]
fn an_unvalidated_external_dotgit_is_refused() {
    let env = TestEnv::new();
    let work = env.work();
    let external = env.tmp().join("external-gitdir");
    dir(&external);
    write(&external.join("HEAD"), "ref: refs/heads/main\n");
    write(&work.join(".git"), &format!("gitdir: {}\n", external.display()));
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(
        &sb,
        &work,
        "mkdir -p .githooks && printf '#!/bin/sh\\n' > .githooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "unvalidated .git file");
    assert!(out.contains("sandbox refused"), "{out}");
    assert!(!work.join(".githooks/pre-commit").exists());
}

/// Threat: a real `.git` has a `commondir` outside the project; its config can name
/// `.githooks` in the writable cwd, and the sandbox does not read that config.
/// Attempt: `commondir` -> an external directory, then write `.githooks/pre-commit`.
/// Expected refusal: a real `.git` with an unprotected common dir is refused.
#[test]
fn a_real_dotgit_with_an_external_commondir_is_refused() {
    let env = TestEnv::new();
    let work = env.work();
    let common = env.tmp().join("external-common");
    dir(&common);
    dir(&work.join(".git/hooks"));
    dir(&work.join(".git/objects"));
    write(&work.join(".git/HEAD"), "ref: refs/heads/main\n");
    write(&work.join(".git/config"), "[core]\n");
    write(&work.join(".git/commondir"), &format!("{}\n", common.display()));
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(
        &sb,
        &work,
        "mkdir -p .githooks && printf '#!/bin/sh\\n' > .githooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "external commondir");
    assert!(out.contains("sandbox refused"), "{out}");
    assert!(!work.join(".githooks/pre-commit").exists());
}

/// Threat: a valid worktree `.git` file names a gitdir outside the project but a common dir
/// inside it; the common dir is exposed by the writable cwd bind and is not self-bound.
/// Attempt: write `common/hooks/pre-commit` behind the worktree layout.
/// Expected refusal: a worktree git/common dir inside what the command can write is refused.
#[test]
fn a_worktree_common_dir_inside_the_project_is_refused() {
    let env = TestEnv::new();
    let work = env.work();
    let gitdir = env.tmp().join("external-gitdir");
    let common = work.join("common");
    dir(&gitdir);
    dir(&common.join("objects"));
    dir(&common.join("worktrees"));
    write(&gitdir.join("HEAD"), "ref: refs/heads/main\n");
    write(
        &gitdir.join("gitdir"),
        &format!("{}\n", work.join(".git").display()),
    );
    write(&gitdir.join("commondir"), &format!("{}\n", common.display()));
    std::os::unix::fs::symlink(&gitdir, common.join("worktrees/external-gitdir")).expect("symlink");
    dir(&common.join("hooks"));
    write(&work.join(".git"), &format!("gitdir: {}\n", gitdir.display()));
    let Some(sb) = sandbox(&env) else { return };
    assert!(
        !sb.layout.git.is_empty(),
        "the fixture must validate as a worktree"
    );
    let (exit, out) = run(
        &sb,
        &work,
        "printf '#!/bin/sh\\n' > common/hooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "worktree common dir inside cwd");
    assert!(out.contains("sandbox refused"), "{out}");
    assert!(!common.join("hooks/pre-commit").exists());
}

/// Threat: a session runs in a nested directory of a repository and the repository's
/// `core.hooksPath` is relative. Git resolves that value from the worktree root, not from the
/// session cwd, so resolving it from the cwd protects a different directory and leaves the
/// real hook directory writable.
/// Attempt: worktree root with `hooksPath = sub/.githooks`; run in `sub`; write the hook.
/// Expected refusal: the worktree-root path `sub/.githooks` is protected, not `sub/sub/...`.
#[test]
fn a_nested_cwd_resolves_a_relative_hooks_path_from_the_worktree_root() {
    let env = TestEnv::new();
    let work = env.work();
    let nested = work.join("sub");
    config(
        &work,
        "[core]\n\trepositoryformatversion = 0\n\thooksPath = sub/.githooks\n",
    );
    // The old cwd-relative resolution derived `sub/sub/.githooks`; keep that path's parent a
    // real directory so the wrong path was protected instead of refused, as the bug did.
    dir(&nested.join("sub"));
    let Some(sb) = sandbox_at(&env, &nested) else {
        return;
    };
    let (exit, out) = run(
        &sb,
        &nested,
        "mkdir -p .githooks && printf '#!/bin/sh\\n' > .githooks/pre-commit",
        true,
        false,
    );
    assert_refused(exit, &out, "nested cwd hooksPath");
    assert!(!nested.join(".githooks/pre-commit").exists());
}

/// Control: a nested session cwd with a relative `hooksPath` whose real target is the worktree
/// root, outside the writable cwd. The sandbox must not derive a path from the session cwd and
/// mask legitimate project content there.
/// Attempt: `hooksPath = .githooks`; run in `sub`; write `sub/.githooks/pre-commit`, which is
/// not the hook directory (git runs `work/.githooks`), so it must be allowed.
#[test]
fn a_nested_cwd_does_not_protect_a_hooks_path_derived_from_the_session_cwd() {
    let env = TestEnv::new();
    let work = env.work();
    let nested = work.join("sub");
    dir(&nested);
    config(
        &work,
        "[core]\n\trepositoryformatversion = 0\n\thooksPath = .githooks\n",
    );
    // The real hook directory, outside the nested writable cwd: the command cannot write it
    // (the worktree root is read-only inside the sandbox), so no mask is needed there.
    dir(&work.join(".githooks"));
    let Some(sb) = sandbox_at(&env, &nested) else {
        return;
    };
    let (exit, out) = run(
        &sb,
        &nested,
        "mkdir -p .githooks && printf '#!/bin/sh\\n' > .githooks/pre-commit",
        true,
        false,
    );
    assert_ok(exit, &out, "nested cwd unrelated .githooks");
    assert!(nested.join(".githooks/pre-commit").exists());
}

/// Threat: moving a writable ancestor moves the read-only hook mount out of Git's path.
/// Attempt: replace each ancestor of a nested hooksPath, then plant a replacement hook.
/// Expected refusal: ancestors are pinned, while unrelated project files remain writable.
#[test]
fn a_custom_hooks_ancestor_cannot_be_replaced() {
    for (hooks, ancestor) in [("sub/hooks", "sub"), ("a/b/hooks", "a"), ("a/b/hooks", "a/b")] {
        let env = TestEnv::new();
        let work = env.work();
        config(
            &work,
            &format!("[core]\n\trepositoryformatversion = 0\n\thooksPath = {hooks}\n"),
        );
        dir(&work.join(hooks));
        write(&work.join(hooks).join("pre-commit"), "original");
        let Some(sb) = sandbox(&env) else { return };
        let command = format!(
            "mv {ancestor} {ancestor}.moved && mkdir -p {hooks} && printf replacement > {hooks}/pre-commit"
        );
        let (exit, out) = run(&sb, &work, &command, true, false);
        assert_refused(exit, &out, "replace a custom hooks ancestor");
        assert!(!work.join(format!("{ancestor}.moved")).exists());
        assert_eq!(
            std::fs::read_to_string(work.join(hooks).join("pre-commit")).unwrap(),
            "original"
        );
        let parent = Path::new(hooks).parent().unwrap().display();
        let (exit, out) = run(
            &sb,
            &work,
            &format!("printf ordinary > {parent}/generated.txt"),
            true,
            false,
        );
        assert_ok(exit, &out, "ordinary build output beside hooks");
    }
}

/// Threat: a read-only custom hook is a symlink to a writable project payload.
/// Attempt: overwrite the payload, with the hook directory inside or outside the project.
/// Expected refusal: the alias cannot make host Git execute changed project content.
#[test]
fn a_custom_hook_symlink_to_a_writable_payload_is_refused() {
    for outside in [false, true] {
        let env = TestEnv::new();
        let work = env.work();
        let hooks = if outside {
            env.root().join("external-hooks")
        } else {
            work.join(".githooks")
        };
        config(
            &work,
            &format!(
                "[core]\n\trepositoryformatversion = 0\n\thooksPath = {}\n",
                hooks.display()
            ),
        );
        dir(&hooks);
        write(&work.join("payload"), "original");
        std::os::unix::fs::symlink(work.join("payload"), hooks.join("pre-commit")).unwrap();
        let Some(sb) = sandbox(&env) else { return };
        let (exit, out) = run(&sb, &work, "printf replacement > payload", true, false);
        assert_refused(exit, &out, "custom hook's writable symlink target");
        assert_eq!(std::fs::read_to_string(work.join("payload")).unwrap(), "original");
    }
}

/// Threat: a custom hook has another hard-link name in the writable project.
/// Attempt: overwrite that name, bypassing the directory's read-only mount.
/// Expected refusal: multiply linked executable hook content cannot be changed.
#[test]
fn a_custom_hook_hard_link_to_a_writable_payload_is_refused() {
    let env = TestEnv::new();
    let work = env.work();
    config(
        &work,
        "[core]\n\trepositoryformatversion = 0\n\thooksPath = .githooks\n",
    );
    dir(&work.join(".githooks"));
    write(&work.join("payload"), "original");
    std::fs::hard_link(work.join("payload"), work.join(".githooks/pre-commit")).unwrap();
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(&sb, &work, "printf replacement > payload", true, false);
    assert_refused(exit, &out, "custom hook's writable hard-link alias");
    assert_eq!(std::fs::read_to_string(work.join("payload")).unwrap(), "original");
}

/// Control: a custom hook may link to an immutable file outside every writable root.
/// Attempt: write unrelated project content with that hook installed.
/// Expected success: safe read-only aliases do not refuse normal project work.
#[test]
fn a_custom_hook_symlink_to_a_read_only_payload_allows_project_work() {
    let env = TestEnv::new();
    let work = env.work();
    config(
        &work,
        "[core]\n\trepositoryformatversion = 0\n\thooksPath = .githooks\n",
    );
    dir(&work.join(".githooks"));
    let payload = env.root().join("read-only-hook");
    write(&payload, "original");
    std::os::unix::fs::symlink(&payload, work.join(".githooks/pre-commit")).unwrap();
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(&sb, &work, "printf ordinary > generated.txt", true, false);
    assert_ok(exit, &out, "safe custom-hook alias");
    assert_eq!(std::fs::read_to_string(payload).unwrap(), "original");
}

/// Threat: a plan-mode command can replace a hooks ancestor through a writable cache bind.
/// Attempt: rename the cache's nested parent and plant a new hook.
/// Expected refusal: cache ancestors are pinned, but ordinary cache output still works.
#[test]
fn custom_hook_ancestors_in_a_plan_cache_cannot_be_replaced() {
    let env = TestEnv::new();
    let work = env.work();
    let cache = env.tmp().join("hook-cache");
    let hooks = cache.join("sub/hooks");
    config(
        &work,
        &format!(
            "[core]\n\trepositoryformatversion = 0\n\thooksPath = {}\n",
            hooks.display()
        ),
    );
    dir(&hooks);
    write(&hooks.join("pre-commit"), "original");
    let Some(mut sb) = sandbox(&env) else { return };
    sb.layout.write.push(cache.clone());
    let command = format!(
        "mv {0}/sub {0}/moved && mkdir -p {0}/sub/hooks && printf replacement > {0}/sub/hooks/pre-commit",
        cache.display()
    );
    let (exit, out) = run(&sb, &work, &command, false, false);
    assert_refused(exit, &out, "plan cache hooks ancestor");
    assert!(!cache.join("moved").exists());
    let (exit, out) = run(
        &sb,
        &work,
        &format!("printf ordinary > {}/sub/generated", cache.display()),
        false,
        false,
    );
    assert_ok(exit, &out, "ordinary plan cache output");
}

/// Threat: a linked worktree exposes its external common git directory read-write.
/// Attempt: change a custom hook there; also try writing the common info directory.
/// Expected refusal: custom hooks and #105 info protection survive; project writes work.
#[test]
fn custom_hooks_in_a_worktree_common_dir_stay_protected() {
    for relative in ["cache/sub/hooks", "info/custom-hooks"] {
        let env = TestEnv::new();
        let work = env.work();
        let common = env.tmp().join("common");
        let gitdir = common.join("worktrees/linked");
        let hooks = common.join(relative);
        dir(&gitdir);
        dir(&common.join("objects"));
        dir(&common.join("hooks"));
        dir(&hooks);
        write(&common.join("info/exclude"), "keep");
        write(
            &common.join("config"),
            &format!(
                "[core]\n\trepositoryformatversion = 0\n\thooksPath = {}\n",
                hooks.display()
            ),
        );
        write(&gitdir.join("HEAD"), "ref: refs/heads/main\n");
        write(
            &gitdir.join("gitdir"),
            &format!("{}\n", work.join(".git").display()),
        );
        write(&gitdir.join("commondir"), "../..\n");
        write(&work.join(".git"), &format!("gitdir: {}\n", gitdir.display()));
        write(&hooks.join("pre-commit"), "original");
        let Some(sb) = sandbox(&env) else { return };
        assert!(
            !sb.layout.git.is_empty(),
            "must exercise a validated linked worktree"
        );
        for path in [hooks.join("pre-commit"), common.join("info/exclude")] {
            let (exit, out) = run(
                &sb,
                &work,
                &format!("printf replacement > {}", path.display()),
                true,
                false,
            );
            assert_refused(exit, &out, "common-dir protected entry");
        }
        assert_eq!(
            std::fs::read_to_string(hooks.join("pre-commit")).unwrap(),
            "original"
        );
        assert_eq!(
            std::fs::read_to_string(common.join("info/exclude")).unwrap(),
            "keep"
        );
        let (exit, out) = run(&sb, &work, "printf ordinary > generated.txt", true, false);
        assert_ok(exit, &out, "ordinary worktree output");
    }
}

/// Control: hooksPath may name the project root, which the sandbox makes read-only.
/// Attempt: read an ordinary project file while Git's own data has nested mounts.
/// Expected success: a later hook mask must override those earlier writable mounts.
#[test]
fn a_project_root_hooks_path_allows_read_only_work() {
    let env = TestEnv::new();
    let work = env.work();
    config(&work, "[core]\n\trepositoryformatversion = 0\n\thooksPath = .\n");
    write(&work.join("ordinary"), "keep");
    write(&work.join(".git/index"), "synthetic-index");
    let Some(sb) = sandbox(&env) else { return };
    let (exit, out) = run(&sb, &work, "cat ordinary", true, false);
    assert_ok(exit, &out, "read-only project-root hook control");
    assert!(out.contains("keep"));
}

/// Control: an explicit write bind lies below a configured hooks directory outside cwd.
/// Attempt: write hook helper content and then unrelated project output.
/// Expected: the late hook mask removes cache writes, but ordinary project work still runs.
#[test]
fn a_write_bind_below_custom_hooks_does_not_refuse_unrelated_work() {
    let env = TestEnv::new();
    let work = env.work();
    let hooks = env.tmp().join("external-hooks");
    let cache = hooks.join("helpers");
    dir(&cache);
    write(&cache.join("payload"), "original");
    config(
        &work,
        &format!(
            "[core]\n\trepositoryformatversion = 0\n\thooksPath = {}\n",
            hooks.display()
        ),
    );
    let Some(mut sb) = sandbox(&env) else { return };
    sb.layout.write.push(cache.clone());
    let (exit, out) = run(
        &sb,
        &work,
        &format!("printf replacement > {}/payload", cache.display()),
        true,
        false,
    );
    assert_refused(exit, &out, "write bind inside configured hooks");
    assert_eq!(
        std::fs::read_to_string(cache.join("payload")).unwrap(),
        "original"
    );
    let (exit, out) = run(&sb, &work, "printf ordinary > generated.txt", true, false);
    assert_ok(exit, &out, "ordinary work outside configured hook tree");
}
