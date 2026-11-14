//! A9: the bwrap argv. Owner: task A9. Signatures below are frozen; bodies are yours.
use crate::gitdirs::{
    alias_refusal, configured_hooks_paths, is_real_dir, protected_entries, unsafe_reason, Mounts,
};
use crate::layout::{
    home_protected_paths, is_home_itself, is_home_or_above, protected_paths, sensitive_paths, Layout,
};
use crate::run::run_argv;
use crate::Bwrap;
use oxa_proto::{codes, CancelToken, ExecSpec, OxaError};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Fixed base environment inside the sandbox (the `PATH` is the layout's, B2).
const BASE_LANG: &str = "C.UTF-8";
const BASE_TERM: &str = "dumb";
/// Seconds the probe may take before it counts as a failure.
const PROBE_TIMEOUT_S: u32 = 10;

const MISSING_HINT: &str =
    "install bubblewrap (the `bubblewrap` package) and make sure unprivileged user namespaces are enabled";
const UNUSABLE_HINT: &str = "check that unprivileged user namespaces are allowed: `sysctl kernel.unprivileged_userns_clone` must be 1, \
and on Ubuntu-based systems `sysctl kernel.apparmor_restrict_unprivileged_userns` must be 0; the `bubblewrap` package must be installed";

/// The exact bwrap argv (without `bwrap` itself) for `s` with the minimal layout (base `PATH`,
/// no toolchain binds, the deny-list of `home`): [`bwrap_args_with`].
pub fn bwrap_args(s: &ExecSpec, home: &Path) -> Vec<String> {
    bwrap_args_with(s, home, &Layout::default())
}

/// The exact bwrap argv (without `bwrap` itself) for `s`. Pure over the file system state and
/// deterministic; golden per case. Order: the fixed flags (`--unshare-all`, `--share-net` only
/// when `net`, `--die-with-parent`, `--new-session`, `--cap-drop ALL`), `--ro-bind / /`,
/// `--dev /dev`, `--proc /proc`, `--tmpfs /tmp`, `--tmpfs <home>`, the cwd (`--bind`, or
/// `--ro-bind` when not writable or when cwd is `$HOME` / `/` / a parent of `$HOME`, B3; in a
/// home workspace (O-199, [`cwd_mode`]) followed by `--bind` of each of
/// [`home_workspace_entries`]), the layout's binds sorted by path (read-only toolchains, read-write
/// caches, B2; a bind that holds the cwd is left out), the git data outside the cwd (sbx-git:
/// read-write dirs (read-only when one of them has a protected entry that is not plain,
/// sbx-gitsym), read-only alternates, then the protected entries: read-only when they exist,
/// masked when missing in a writable sandbox), the protected project files of a writable cwd
/// read-only (B4; a cwd `.git` directory is first bound onto itself so it cannot be renamed,
/// read-only when its protected entries are not plain (sbx-gitsym, as the git dirs above);
/// for it also its missing `hooks` / `config` and its
/// `config.worktree` (sbx-hooks, `gitdirs.rs`), and a `.git` file is read-only), then the
/// deny-list masks sorted by path (B3: a read-only empty
/// tmpfs over a directory, `/dev/null` over a file; only the entries that exist and that a
/// bind would show), `--chdir <cwd>`, `--clearenv` + `--setenv` for the base environment
/// (`PATH` of the layout, `HOME`, `LANG`, `TERM=dumb`, `AI_AGENT=oxa`), the layout's
/// passed-through variables (sorted), then `s.env` in order, then `-- bash -c <command>`.
pub fn bwrap_args_with(s: &ExecSpec, home: &Path, layout: &Layout) -> Vec<String> {
    let home_s = home.to_string_lossy().into_owned();
    // sbx-symhome: every decision and every mount uses the canonical cwd and home, so a cwd
    // that only resolves to `$HOME` (or into it) gets the masks of the real one. An
    // unresolvable path stays as given here; `refusal` refuses to run then (fail closed).
    let home_c = canon(home);
    let cwd_c = canon(&s.cwd);
    let cwd_path = cwd_c.as_path();
    let cwd = cwd_c.to_string_lossy().into_owned();
    let mut a: Vec<String> = Vec::new();
    push(&mut a, &["--unshare-all"]);
    if s.net {
        push(&mut a, &["--share-net"]);
    }
    push(
        &mut a,
        &["--die-with-parent", "--new-session", "--cap-drop", "ALL"],
    );
    push(
        &mut a,
        &["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"],
    );
    push(&mut a, &["--tmpfs", "/tmp"]);
    push(&mut a, &["--tmpfs", &home_c.to_string_lossy()]);
    let mode = cwd_mode(s, home, layout);
    let writable = mode == CwdMode::Writable;
    let bind = if writable { "--bind" } else { "--ro-bind" };
    push(&mut a, &[bind, &cwd, &cwd]);
    // O-199: a home workspace: the home root stays read-only, its plain folders and files
    // become writable (never a dot-entry, a symlink or a protected name such as `~/bin`).
    let home_entries = if mode == CwdMode::HomeWorkspace {
        home_workspace_entries(&home_c, layout)
    } else {
        Vec::new()
    };
    for p in &home_entries {
        let p = p.to_string_lossy();
        push(&mut a, &["--bind", &p, &p]);
    }
    // B2: toolchains read-only, caches read-write, in path order (a parent before its child).
    let mut binds: Vec<(&Path, bool)> = layout
        .read
        .iter()
        .map(|p| (p.as_path(), false))
        .chain(layout.write.iter().map(|p| (p.as_path(), true)))
        .filter(|(p, _)| !cwd_path.starts_with(p) && !cwd_path.starts_with(canon(p)))
        .collect();
    binds.sort();
    for (p, rw) in &binds {
        let p = p.to_string_lossy();
        push(&mut a, &[if *rw { "--bind" } else { "--ro-bind" }, &p, &p]);
    }
    // sbx-git: the git data of a worktree or shared clone outside the cwd (resolved once when
    // the session's sandbox is built). Read-only when the command may not write; alternates and
    // the protected hooks and config are read-only in any case.
    // sbx-gitsym: checked again for every command (the session may run long): a git directory
    // whose protected entries are not plain (`gitdirs::unsafe_reason`) is never read-write, and
    // then none of them is (no masks either: nothing can be created in a read-only bind).
    let git_writable = writable && layout.git.rw.iter().all(|d| unsafe_reason(d).is_none());
    let git_rw = if git_writable { "--bind" } else { "--ro-bind" };
    for p in layout
        .git
        .rw
        .iter()
        .filter(|p| !p.starts_with(cwd_path) && !cwd_path.starts_with(p))
    {
        let p = p.to_string_lossy();
        push(&mut a, &[git_rw, &p, &p]);
    }
    for p in layout
        .git
        .ro
        .iter()
        .chain(&layout.git.protect)
        .filter(|p| !p.starts_with(cwd_path) && !cwd_path.starts_with(p))
    {
        if std::fs::symlink_metadata(p).is_ok() {
            let p = p.to_string_lossy();
            push(&mut a, &["--ro-bind", &p, &p]);
        } else if git_writable {
            // sbx-hooks: a missing hook dir or config file cannot be created either.
            mask_missing(&mut a, p);
        }
    }
    // B4: what runs outside the sandbox later stays as it is.
    if writable {
        let dotgit = cwd_path.join(".git");
        // sbx-hooks: the cwd's `.git` directory is a mount point (bound onto itself), so a
        // command cannot rename it away and put a copy with writable hooks in its place
        // (renaming an ancestor of a read-only mount is allowed, renaming a mount point is not).
        // sbx-gitsym: a `.git` whose protected entries are not plain is bound read-only instead.
        let dotgit_writable = is_real_dir(&dotgit) && unsafe_reason(&dotgit).is_none();
        if is_real_dir(&dotgit) {
            let p = dotgit.to_string_lossy();
            push(
                &mut a,
                &[if dotgit_writable { "--bind" } else { "--ro-bind" }, &p, &p],
            );
        }
        let protected = protected_paths(cwd_path);
        for p in &protected {
            if is_real(p) {
                let p = p.to_string_lossy();
                push(&mut a, &["--ro-bind", &p, &p]);
            }
        }
        // sbx-hooks: the cwd's own git directory, also what does not exist yet; a worktree's
        // `.git` file names the gitdir whose hooks git runs, so it is read-only too.
        if dotgit_writable {
            for p in protected_entries(&dotgit, &dotgit) {
                if !is_real(&p) {
                    mask_missing(&mut a, &p);
                } else if !protected.contains(&p) {
                    let p = p.to_string_lossy();
                    push(&mut a, &["--ro-bind", &p, &p]);
                }
            }
        } else if std::fs::symlink_metadata(&dotgit).is_ok_and(|m| m.is_file()) {
            let p = dotgit.to_string_lossy();
            push(&mut a, &["--ro-bind", &p, &p]);
        }
    }
    // sbx-hooks: git runs the hooks of `core.hooksPath` (a repository config may name a
    // directory inside the project or a writable cache). Protect it even in plan mode: the
    // layout's read-write binds are writable there.
    protect_hooks_paths(&mut a, cwd_path, &home_c, layout, mode);
    // Host runtime IPC is not part of the project and not a network socket, so the network
    // namespace does not contain it. A command can otherwise reach the user's session bus
    // (which starts processes outside the sandbox), the system bus and the Docker/Podman
    // sockets, and systemd-resolved (DNS with the network unshared). Mask them after every
    // bind, before the deny-list.
    if layout.mask_runtime_ipc {
        for p in runtime_ipc_paths(&home_c) {
            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            let ps = p.to_string_lossy();
            if meta.is_dir() {
                push(&mut a, &["--tmpfs", &ps, "--remount-ro", &ps]);
            } else {
                push(&mut a, &["--ro-bind", "/dev/null", &ps]);
            }
        }
    }
    // B3: the deny-list, after every bind so no bind re-exposes it. sbx-symhome: each entry is
    // masked where it is shown: at its own path (as before), at its canonical path when that
    // lies outside the hidden home, and inside every mount whose canonical source holds it
    // (the cwd, the home workspace's entries, the layout's binds at their own destinations,
    // the git data), so a symlink in the way never leaves a second, unmasked route.
    let shown = |p: &Path| {
        (!p.starts_with(home) && !p.starts_with(&home_c))
            || p.starts_with(cwd_path)
            || binds.iter().any(|(b, _)| p.starts_with(b))
            || layout.git.paths().any(|b| p.starts_with(b))
    };
    let mut mounts: Vec<(PathBuf, PathBuf)> = vec![(cwd_c.clone(), cwd_c.clone())];
    mounts.extend(home_entries.iter().map(|p| (p.clone(), canon(p))));
    mounts.extend(binds.iter().map(|(b, _)| (b.to_path_buf(), canon(b))));
    mounts.extend(layout.git.paths().map(|b| (b.to_path_buf(), canon(b))));
    let mut masks: Vec<(PathBuf, bool)> = Vec::new();
    for p in sensitive_paths(home, layout.agent_dir.as_deref()) {
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        let dir = meta.is_dir();
        if shown(&p) {
            masks.push((p.clone(), dir));
        }
        let Ok(pc) = std::fs::canonicalize(&p) else {
            continue;
        };
        if !pc.starts_with(&home_c) && !pc.starts_with(home) {
            masks.push((pc.clone(), dir));
        }
        for (dest, src) in &mounts {
            if let Ok(rest) = pc.strip_prefix(src) {
                masks.push((dest.join(rest), dir));
            }
        }
    }
    masks.sort();
    masks.dedup_by(|x, y| x.0 == y.0);
    let mut hidden: Vec<PathBuf> = Vec::new();
    for (p, dir) in masks {
        // Inside a directory masked already: hidden, and nothing can be mounted in there.
        if hidden.iter().any(|h| p.starts_with(h)) {
            continue;
        }
        let ps = p.to_string_lossy();
        if dir {
            push(&mut a, &["--tmpfs", &ps, "--remount-ro", &ps]);
            hidden.push(p);
        } else {
            push(&mut a, &["--ro-bind", "/dev/null", &ps]);
        }
    }
    push(&mut a, &["--chdir", &cwd, "--clearenv"]);
    push(&mut a, &["--setenv", "PATH", &layout.path_value()]);
    push(&mut a, &["--setenv", "HOME", &home_s]);
    push(&mut a, &["--setenv", "LANG", BASE_LANG]);
    push(&mut a, &["--setenv", "TERM", BASE_TERM]);
    push(&mut a, &["--setenv", "AI_AGENT", oxa_proto::names::AI_AGENT]);
    for (k, v) in &layout.env {
        push(&mut a, &["--setenv", k, v]);
    }
    for (k, v) in &s.env {
        push(&mut a, &["--setenv", k, v]);
    }
    push(&mut a, &["--", "bash", "-c", &s.command]);
    a
}

/// `p` canonical, or `p` itself when it cannot be resolved (the argv stays deterministic; the
/// sandbox refuses to run a command then, [`refusal`]).
fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// A real file or directory (not a symlink: binding over one would follow it).
fn is_real(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| !m.file_type().is_symlink())
}

/// sbx-hooks: keep the missing protected git entry `p` missing for the command: an empty
/// read-only tmpfs for the `hooks` directory, `/dev/null` for a file. bwrap creates the mount
/// point (an empty directory or file) in the host's git directory, which stays after the run.
/// A file is normally created empty before the run ([`create_missing_git_files`]) and bound
/// read-only as an existing entry; the `/dev/null` mask is the fallback for an argv built
/// without that step (still safe, but git cannot read a device node on a read-only bind, and
/// it refuses an unreadable `config`). Nothing when the parent is not a real directory (never
/// through a symlink).
fn mask_missing(a: &mut Vec<String>, p: &Path) {
    if !p.parent().is_some_and(is_real_dir) {
        return;
    }
    let ps = p.to_string_lossy();
    if p.file_name().is_some_and(|n| n == "hooks") {
        push(a, &["--tmpfs", &ps, "--remount-ro", &ps]);
    } else {
        push(a, &["--ro-bind", "/dev/null", &ps]);
    }
}

/// sbx-hooks: before a writable command runs, create every missing protected git *file* of
/// [`bwrap_args_with`] (`config`, `config.worktree`) empty on the host, so the argv binds a
/// readable empty file read-only (git reads it as no settings). Only in a real parent
/// directory, never over anything that exists (`create_new` does not follow a symlink); errors
/// are ignored (the argv then masks the file with `/dev/null`). The `hooks` directory needs
/// no such step: its tmpfs mask is readable.
pub(crate) fn create_missing_git_files(s: &ExecSpec, home: &Path, layout: &Layout) {
    let cwd_c = canon(&s.cwd);
    let cwd = cwd_c.as_path();
    if cwd_mode(s, home, layout) != CwdMode::Writable {
        return;
    }
    // sbx-gitsym: nothing is created for git directories that are bound read-only.
    let git_writable = layout.git.rw.iter().all(|d| unsafe_reason(d).is_none());
    let mut files: Vec<PathBuf> = layout
        .git
        .protect
        .iter()
        .filter(|p| git_writable && !p.starts_with(cwd) && !cwd.starts_with(p))
        .cloned()
        .collect();
    let dotgit = cwd.join(".git");
    if is_real_dir(&dotgit) && unsafe_reason(&dotgit).is_none() {
        files.extend(protected_entries(&dotgit, &dotgit));
    }
    for p in files {
        if p.file_name().is_some_and(|n| n == "hooks") || !p.parent().is_some_and(is_real_dir) {
            continue;
        }
        let _ = std::fs::OpenOptions::new().write(true).create_new(true).open(&p);
    }
}

/// sbx-gitsym: why the command `s` must not run at all, or `None`. Only for a repository whose
/// git directories are bound read-only because a protected entry is not plain
/// (`gitdirs::unsafe_reason`): then every alias of its protected entries is followed
/// (`gitdirs::alias_refusal`) against what the command could still write (the writable cwd, the
/// layout's read-write binds), with the git directories read-only.
pub(crate) fn refusal(s: &ExecSpec, home: &Path, layout: &Layout) -> Option<String> {
    // sbx-symhome: the masks are decided on canonical paths; without them, nothing runs.
    for (what, p) in [("working directory", s.cwd.as_path()), ("home directory", home)] {
        if let Err(e) = std::fs::canonicalize(p) {
            return Some(format!(
                "sandbox refused: the {what} {} cannot be resolved ({e}), so its protections \
cannot be checked; commands are not run",
                p.display()
            ));
        }
    }
    let cwd = canon(&s.cwd);
    let mode = cwd_mode(s, home, layout);
    let writable = mode == CwdMode::Writable;
    let dotgit = cwd.join(".git");
    // sbx-hooks/sbx-gitsym: a `.git` that is not a real directory cannot be pinned and
    // protected. A symlink can be repointed at a writable git directory, and a `.git` file
    // can name one inside the cwd (a worktree in the project itself, or a forged `gitdir:`
    // line): its `config` and `hooks` would stay writable while git outside the sandbox runs
    // them. Both are refused rather than guessed at.
    if let Ok(meta) = std::fs::symlink_metadata(&dotgit) {
        if meta.file_type().is_symlink() {
            return Some(format!(
                "sandbox refused: {} is a symlink, so the git directory it names cannot be \
protected; commands are not run here",
                dotgit.display()
            ));
        }
        if meta.is_dir() {
            // A real `.git` can still move its common dir elsewhere (a linked-worktree
            // layout). Git reads hooks and config from there.
            if let Some(target) = commondir_target(&dotgit) {
                let home_entries: Vec<PathBuf> = if mode == CwdMode::HomeWorkspace {
                    home_workspace_entries(&canon(home), layout)
                        .iter()
                        .map(|p| canon(p))
                        .collect()
                } else {
                    Vec::new()
                };
                let reachable = target.starts_with(&cwd)
                    || layout.write.iter().any(|w| target.starts_with(canon(w)))
                    || home_entries.iter().any(|e| target.starts_with(e));
                if reachable {
                    return Some(format!(
                        "sandbox refused: {} names the common dir {}, which lies in what the \
command can write; its hooks and config could be changed, so commands are not run here",
                        dotgit.join("commondir").display(),
                        target.display()
                    ));
                }
                // A real `.git` with a common dir is not the plain repository layout the
                // sandbox protects (a linked worktree has a `.git` file, not a dir). Its
                // config is not read here, so a hooks path in it cannot be checked.
                return Some(format!(
                    "sandbox refused: {} names the common dir {}, which the sandbox does not \
protect for a real .git directory; its hooks and config cannot be checked, so commands \
are not run here",
                    dotgit.join("commondir").display(),
                    target.display()
                ));
            }
        }
        if meta.is_file() {
            if let Some(target) = dotgit_file_target(&dotgit) {
                let home_entries: Vec<PathBuf> = if mode == CwdMode::HomeWorkspace {
                    home_workspace_entries(&canon(home), layout)
                        .iter()
                        .map(|p| canon(p))
                        .collect()
                } else {
                    Vec::new()
                };
                let reachable = target.starts_with(&cwd)
                    || layout.write.iter().any(|w| target.starts_with(canon(w)))
                    || home_entries.iter().any(|e| target.starts_with(e));
                // A `.git` file that `gitdirs::resolve` did not validate is not bound; its
                // config could still name a hooks path inside this writable cwd, so it cannot
                // be checked. Refuse rather than guess.
                let validated = layout
                    .git
                    .dirs
                    .iter()
                    .any(|d| target == *d || target.starts_with(d));
                if reachable || !validated {
                    return Some(format!(
                        "sandbox refused: the .git file names {}, which is not a git directory \
the sandbox validated, or lies in what the command can write; its hooks and config cannot \
be protected, so commands are not run here",
                        target.display()
                    ));
                }
            }
        }
    }
    // A validated worktree can still name a common dir inside the cwd. The cwd bind exposes
    // it, and only the plain `.git` directory layout is self-bound and protected. Refuse any
    // worktree git dir (or common dir) that lies in what the command can write.
    if !is_real_dir(&dotgit) {
        let home_entries: Vec<PathBuf> = if mode == CwdMode::HomeWorkspace {
            home_workspace_entries(&canon(home), layout)
                .iter()
                .map(|p| canon(p))
                .collect()
        } else {
            Vec::new()
        };
        for d in &layout.git.dirs {
            let reachable = d.starts_with(&cwd)
                || layout.write.iter().any(|w| d.starts_with(canon(w)))
                || home_entries.iter().any(|e| d.starts_with(e));
            if reachable {
                return Some(format!(
                    "sandbox refused: the worktree's git directory {} lies in what the command \
can write; its hooks and config cannot be protected, so commands are not run here",
                    d.display()
                ));
            }
        }
    }
    // sbx-hooks: a `core.hooksPath` inside what the command can write is protected by
    // `protect_hooks_paths`. A symlink there, or a missing parent directory, cannot be
    // pinned read-only; refuse rather than let a command replace it.
    let home_c = canon(home);
    let roots = writable_roots(&cwd, &home_c, layout, mode);
    for p in configured_hook_paths(&cwd, &home_c, layout) {
        let Some(root) = under_any(&p, &roots) else {
            continue;
        };
        let symlink = std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink());
        if symlink || has_symlink_ancestor(&p, root) {
            return Some(format!(
                "sandbox refused: the repository's core.hooksPath {} is a symlink or lies below one, so its hooks cannot be pinned; commands are not run here",
                p.display()
            ));
        }
        if std::fs::symlink_metadata(&p).is_err() && !p.parent().is_some_and(is_real_dir) {
            return Some(format!(
                "sandbox refused: the repository's core.hooksPath {} does not exist and its parent is not a real directory, so it cannot be masked; commands are not run here",
                p.display()
            ));
        }
    }
    let mut dirs = layout.git.dirs.clone();
    if is_real_dir(&dotgit) {
        dirs.push(canon(&dotgit));
    }
    dirs.sort();
    dirs.dedup();
    if dirs.iter().all(|d| unsafe_reason(d).is_none()) {
        return None;
    }
    let mut rw: Vec<PathBuf> = layout.write.iter().map(|p| canon(p)).collect();
    if writable {
        rw.push(cwd.clone());
    }
    if mode == CwdMode::HomeWorkspace {
        rw.extend(
            home_workspace_entries(&canon(home), layout)
                .iter()
                .map(|p| canon(p)),
        );
    }
    let dotgit_c = canon(&dotgit);
    let ro: Vec<PathBuf> = dirs
        .iter()
        .filter(|d| (!d.starts_with(&cwd) && !cwd.starts_with(d)) || **d == dotgit_c)
        .cloned()
        .collect();
    alias_refusal(&dirs, &Mounts { rw, ro }, home).map(|why| {
        format!(
            "sandbox refused: git: {why}; a protected git entry (config, hooks) of this repository \
reaches what a command could change, so commands are not run here"
        )
    })
}

/// sbx-ipc: the host runtime sockets a command must not reach. They live outside `$HOME`,
/// so the deny-list does not cover them and `--unshare-net` does not either (a Unix socket is
/// not a network socket).
fn runtime_ipc_paths(home: &Path) -> Vec<PathBuf> {
    let uid = std::fs::metadata(home)
        .map(|m| m.uid())
        .unwrap_or_else(|_| unsafe { libc::geteuid() });
    [
        format!("/run/user/{uid}"),
        "/run/systemd/resolve".to_string(),
        "/run/dbus/system_bus_socket".to_string(),
        "/run/systemd/private".to_string(),
        "/run/docker.sock".to_string(),
        "/run/podman/podman.sock".to_string(),
        "/run/containers".to_string(),
        "/run/nscd/socket".to_string(),
        "/run/avahi-daemon/socket".to_string(),
        "/run/sssd".to_string(),
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect()
}

/// The writable roots of a command: the cwd (only when it is bound read-write), the layout's
/// read-write binds and, for a home workspace, its plain entries.
fn writable_roots(cwd: &Path, home: &Path, layout: &Layout, mode: CwdMode) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if mode == CwdMode::Writable {
        out.push(cwd.to_path_buf());
    }
    out.extend(layout.write.iter().map(|p| canon(p)));
    if mode == CwdMode::HomeWorkspace {
        out.extend(home_workspace_entries(home, layout).iter().map(|p| canon(p)));
    }
    out
}

/// The directory a configured `core.hooksPath` names, resolved as git resolves it: absolute
/// as written, `~/` in the home directory, anything else from the worktree root (not the
/// session cwd, which may be a nested directory).
fn resolve_hooks_path(raw: &Path, home: &Path, worktree: &Path) -> PathBuf {
    let p = if raw == Path::new("~") {
        home.to_path_buf()
    } else if let Some(rest) = raw.to_str().and_then(|s| s.strip_prefix("~/")) {
        home.join(rest)
    } else if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        worktree.join(raw)
    };
    lexical_normalize(&p)
}

/// Every `core.hooksPath` of the repository at `cwd`, from its git directories' configs.
fn configured_hook_paths(cwd: &Path, home: &Path, layout: &Layout) -> Vec<PathBuf> {
    let mut dirs = layout.git.dirs.clone();
    let dotgit = cwd.join(".git");
    if is_real_dir(&dotgit) {
        dirs.push(canon(&dotgit));
    }
    dirs.sort();
    dirs.dedup();
    // A session may run in a nested directory of the worktree; git resolves a relative
    // `core.hooksPath` from the worktree root, so the protection must too.
    let worktree = layout.git.worktree.as_deref().unwrap_or(cwd);
    let mut out: Vec<PathBuf> = dirs
        .iter()
        .flat_map(|d| configured_hooks_paths(d, home))
        .map(|raw| resolve_hooks_path(&raw, home, worktree))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// A symlink between `root` and `p`: a mount on `p` would not pin a directory the command can
/// rename (the target of the link is what git opens).
/// `p` is inside one of `roots`, lexically or after resolving the existing part.
fn under_any<'a>(p: &Path, roots: &'a [PathBuf]) -> Option<&'a PathBuf> {
    roots.iter().find(|r| p.starts_with(r) || canon(p).starts_with(r))
}

fn has_symlink_ancestor(p: &Path, root: &Path) -> bool {
    let Ok(rel) = p.strip_prefix(root) else {
        return false;
    };
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        cur.push(c.as_os_str());
        if std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink()) {
            return true;
        }
    }
    false
}

/// sbx-hooks: bind a repository's `core.hooksPath` directory read-only (or mask it when it
/// does not exist yet) so a command cannot plant a hook that git runs outside the sandbox.
/// Only paths inside what the command can write are touched; a read-only or hidden path needs
/// nothing. A symlink or a missing parent is refused by [`refusal`].
fn protect_hooks_paths(a: &mut Vec<String>, cwd: &Path, home: &Path, layout: &Layout, mode: CwdMode) {
    let roots = writable_roots(cwd, home, layout, mode);
    let protected = protected_paths(cwd);
    for p in configured_hook_paths(cwd, home, layout) {
        let Some(root) = under_any(&p, &roots) else {
            continue;
        };
        if protected.contains(&p) || has_symlink_ancestor(&p, root) {
            continue;
        }
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {}
            Ok(m) if m.is_dir() || m.is_file() => {
                let p = p.to_string_lossy();
                push(a, &["--ro-bind", &p, &p]);
            }
            Ok(_) => {
                let p = p.to_string_lossy();
                push(a, &["--ro-bind", "/dev/null", &p]);
            }
            Err(_) if p.parent().is_some_and(is_real_dir) => {
                let p = p.to_string_lossy();
                push(a, &["--tmpfs", &p, "--remount-ro", &p]);
            }
            Err(_) => {}
        }
    }
}

/// The canonical (or, when it does not exist yet, lexical) directory named by a `.git` file.
/// `None` when the file is unreadable or names nothing; git then has no repository to use.
fn dotgit_file_target(dotgit: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dotgit).ok()?;
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))?
        .trim();
    if line.is_empty() {
        return None;
    }
    let p = Path::new(line);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        dotgit.parent()?.join(p)
    };
    // A missing target cannot be canonicalised; normalise it lexically so `..` cannot make a
    // writable directory look outside the cwd (git would resolve it to the same place).
    Some(std::fs::canonicalize(&abs).unwrap_or_else(|_| lexical_normalize(&abs)))
}

/// The directory a real `.git`'s `commondir` file names (canonical, or lexically normalised
/// when it does not exist yet).
fn commondir_target(dotgit: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dotgit.join("commondir")).ok()?;
    let line = text.lines().next()?.trim();
    if line.is_empty() {
        return None;
    }
    let p = Path::new(line);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        dotgit.join(p)
    };
    Some(std::fs::canonicalize(&abs).unwrap_or_else(|_| lexical_normalize(&abs)))
}

/// `p` with `.` removed and `..` popped, without touching the file system.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
/// How the cwd of a command is bound (B3, O-199).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CwdMode {
    /// A project directory of a writable command: bound read-write (B4 protections inside).
    Writable,
    /// Plan mode, or a home root without a home workspace: bound read-only.
    ReadOnly,
    /// The home directory itself, the user confirmed it (`Layout::allow_home_write`): the
    /// home root and its dot-entries read-only, its plain entries read-write
    /// ([`home_workspace_entries`]), the deny-list masked.
    HomeWorkspace,
}

/// The [`CwdMode`] of `s`. A home root is `/`, the home directory or a parent of it, also when
/// the cwd only resolves to one (a symlink). A parent of the home directory or `/` is never a
/// workspace, whatever the settings.
pub(crate) fn cwd_mode(s: &ExecSpec, home: &Path, layout: &Layout) -> CwdMode {
    if !s.writable {
        return CwdMode::ReadOnly;
    }
    let cwd = s.cwd.as_path();
    let home_root = is_home_or_above(cwd, home) || is_home_or_above(&canon(cwd), &canon(home));
    if !home_root {
        CwdMode::Writable
    } else if layout.allow_home_write && is_home_itself(cwd, home) {
        CwdMode::HomeWorkspace
    } else {
        CwdMode::ReadOnly
    }
}

/// O-199: what bash may write in a home workspace: the plain top-level entries of `home`
/// (real directories and regular files, by name), sorted. Never a dot-entry (shell startup
/// files, `.config`, `.local`, `.ssh`, the agents' settings all live there), a symlink (it may
/// lead out of the home), a name of `oxa_proto::guard::PROTECTED_IN_HOME` (`bin`) or an entry
/// inside the deny-list. The home root itself stays read-only, so nothing new can appear next
/// to the dotfiles.
pub(crate) fn home_workspace_entries(home: &Path, layout: &Layout) -> Vec<PathBuf> {
    let protected = home_protected_paths(home);
    let mut deny = sensitive_paths(home, layout.agent_dir.as_deref());
    deny.extend(
        deny.iter()
            .filter_map(|d| std::fs::canonicalize(d).ok())
            .collect::<Vec<_>>(),
    );
    let Ok(rd) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter(|e| {
            std::fs::symlink_metadata(e.path())
                .is_ok_and(|m| m.file_type().is_dir() || m.file_type().is_file())
        })
        .map(|e| e.path())
        .filter(|p| !protected.contains(p) && !deny.iter().any(|d| p.starts_with(d)))
        .collect();
    out.sort();
    out
}

fn push(a: &mut Vec<String>, xs: &[&str]) {
    a.extend(xs.iter().map(|x| (*x).to_string()));
}

/// The body of `Bwrap::detect` (lib.rs, architect-owned, must delegate here): find `bwrap` on
/// `path_var` and probe it. Failure = `no_sandbox` with a hint.
pub(crate) fn detect_bwrap(path_var: &str, home: &Path) -> Result<Bwrap, OxaError> {
    let bwrap = find_on_path(path_var, "bwrap").ok_or_else(|| {
        OxaError::new(codes::NO_SANDBOX, "bwrap is not available; bash is disabled").with_hint(MISSING_HINT)
    })?;
    probe(&bwrap)?;
    Ok(Bwrap {
        bwrap,
        home: home.to_path_buf(),
        layout: Layout {
            mask_runtime_ipc: true,
            ..Layout::default()
        },
    })
}

/// The body of `Sandbox::check` for [`Bwrap`] (lib.rs, architect-owned, must delegate here):
/// re-run the probe for `b.bwrap`.
pub(crate) fn check_bwrap(b: &Bwrap) -> Result<(), OxaError> {
    probe(&b.bwrap)
}

/// The first absolute PATH entry holding an executable `name`. Relative entries are ignored.
fn find_on_path(path_var: &str, name: &str) -> Option<PathBuf> {
    path_var
        .split(':')
        .filter(|dir| dir.starts_with('/'))
        .map(|dir| Path::new(dir).join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Run `bwrap --unshare-all --ro-bind / / true`; anything but a clean exit is `no_sandbox`.
fn probe(bwrap: &Path) -> Result<(), OxaError> {
    let argv: Vec<String> = [
        bwrap.to_string_lossy().as_ref(),
        "--unshare-all",
        "--ro-bind",
        "/",
        "/",
        "true",
    ]
    .iter()
    .map(|x| (*x).to_string())
    .collect();
    let r = run_argv(
        &argv,
        &[],
        Some(PROBE_TIMEOUT_S),
        &CancelToken::new(),
        &mut |_| {},
    );
    if r.exit == Some(0) {
        return Ok(());
    }
    let why = if r.timed_out {
        "the probe timed out".to_string()
    } else {
        String::from_utf8_lossy(&r.output).trim().to_string()
    };
    let message = if why.is_empty() {
        "bwrap is not usable; bash is disabled".to_string()
    } else {
        format!("bwrap is not usable; bash is disabled: {why}")
    };
    Err(OxaError::new(codes::NO_SANDBOX, message).with_hint(UNUSABLE_HINT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_on_path_skips_relative_and_empty_entries() {
        assert_eq!(find_on_path("", "bwrap"), None);
        assert_eq!(find_on_path("relative:", "bwrap"), None);
    }
}

#[cfg(test)]
mod git_files_tests {
    use super::*;
    use oxa_fake::TestEnv;

    fn spec(cwd: &Path, writable: bool) -> ExecSpec {
        ExecSpec {
            command: "true".into(),
            cwd: cwd.to_path_buf(),
            writable,
            net: false,
            timeout_s: None,
            env: Vec::new(),
        }
    }

    #[test]
    fn missing_config_is_created_empty_and_then_bound_read_only() {
        let env = TestEnv::new();
        env.write("work/.git/HEAD", "ref: refs/heads/main\n");
        let work = env.work();
        let layout = Layout::default();
        create_missing_git_files(&spec(&work, false), &env.home(), &layout);
        assert!(!work.join(".git/config").exists(), "plan mode creates nothing");
        create_missing_git_files(&spec(&work, true), &env.home(), &layout);
        assert_eq!(std::fs::read(work.join(".git/config")).expect("created"), b"");
        assert!(!work.join(".git/hooks").exists(), "hooks are masked, not created");
        let a = bwrap_args_with(&spec(&work, true), &env.home(), &layout);
        let cfg = work.join(".git/config").to_string_lossy().into_owned();
        assert!(a.windows(3).any(|w| w == ["--ro-bind", &cfg, &cfg]), "{a:?}");
        assert!(!a.iter().any(|x| x == "/dev/null"), "{a:?}");
    }

    #[test]
    fn existing_files_and_symlinks_are_left_alone() {
        let env = TestEnv::new();
        env.write("work/.git/config", "[core]\n\tworktreeConfig = true\n");
        let target = env.write("elsewhere/target", "keep");
        let work = env.work();
        std::os::unix::fs::symlink(&target, work.join(".git/config.worktree")).expect("symlink");
        create_missing_git_files(&spec(&work, true), &env.home(), &Layout::default());
        assert_eq!(std::fs::read_to_string(&target).expect("target"), "keep");
        assert_eq!(
            std::fs::read_to_string(work.join(".git/config")).expect("config"),
            "[core]\n\tworktreeConfig = true\n"
        );
    }
}

#[cfg(test)]
mod detect_tests {
    use super::*;
    use oxa_fake::TestEnv;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    /// Writing a script and then executing it races with a fork in another test thread (the
    /// forked child briefly holds the write fd: ETXTBSY). Tests that do both take this lock.
    static SCRIPT_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn empty_path_is_no_sandbox() {
        let env = TestEnv::new();
        let err = detect_bwrap("", &env.home()).expect_err("no bwrap on an empty PATH");
        assert_eq!(err.code, codes::NO_SANDBOX);
        assert_eq!(err.message, "bwrap is not available; bash is disabled");
        assert!(err.hint.as_deref().is_some_and(|h| h.contains("bubblewrap")));
    }

    #[test]
    fn failing_probe_is_no_sandbox_with_its_output() {
        let _guard = SCRIPT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = TestEnv::new();
        let script = env.write(
            "bin/bwrap",
            "#!/bin/sh\necho 'setting up uid map: Permission denied' >&2\nexit 1\n",
        );
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let dir = script.parent().expect("dir").to_string_lossy().into_owned();
        let err = detect_bwrap(&dir, &env.home()).expect_err("a failing probe is no sandbox");
        assert_eq!(err.code, codes::NO_SANDBOX);
        assert!(err.message.contains("Permission denied"), "{}", err.message);
        assert!(err.hint.as_deref().is_some_and(|h| h.contains("userns")));
    }

    #[test]
    fn a_working_probe_detects_and_keeps_home() {
        let _guard = SCRIPT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = TestEnv::new();
        let script = env.write("bin/bwrap", "#!/bin/sh\nexit 0\n");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let dir = script.parent().expect("dir").to_string_lossy().into_owned();
        let b = detect_bwrap(&dir, &env.home()).expect("probe passes");
        assert_eq!(b.bwrap, script);
        assert_eq!(b.home, env.home());
        assert!(check_bwrap(&b).is_ok());
    }
}
