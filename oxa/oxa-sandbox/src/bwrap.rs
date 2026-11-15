//! A9: the bwrap argv. Owner: task A9. Signatures below are frozen; bodies are yours.
use crate::gitdirs::{
    alias_refusal, configured_hooks_paths, hooks_alias_refusal, is_real_dir, protected_entries,
    unsafe_reason, Mounts,
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
    bwrap_args_homes(s, home, layout, real_home().as_deref())
}

/// [`bwrap_args_with`] with the real account home given explicitly (tests).
/// #280: only a lone, plain `git merge|pull|checkout|switch|stash|rebase|reset|cherry-pick ...`
/// may write the project config; any shell syntax, quoting, `git -c`, or a command-running
/// option (`--exec`, `-x`) keeps the read-only protection.
pub(crate) fn is_plain_git_update(command: &str) -> bool {
    if command.chars().any(|c| {
        !(c.is_ascii_alphanumeric()
            || matches!(c, ' ' | '-' | '_' | '.' | '/' | '=' | ':' | '@' | '^' | '~' | ','))
    }) {
        return false;
    }
    let mut t = command.split_whitespace();
    if t.next() != Some("git") {
        return false;
    }
    let ok = matches!(
        t.next(),
        Some("merge" | "pull" | "checkout" | "switch" | "stash" | "rebase" | "reset" | "cherry-pick")
    );
    ok && !t.any(|a| a == "-x" || a.starts_with("--exec") || a.starts_with("--config") || a == "-c")
}

pub(crate) fn bwrap_args_homes(
    s: &ExecSpec,
    home: &Path,
    layout: &Layout,
    real_home: Option<&Path>,
) -> Vec<String> {
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
    // #53: a `$HOME` that is not the account's real home leaves the real one visible. Hide it
    // like `$HOME` (an empty tmpfs; the cwd and the binds below are mounted on top), so its
    // credentials are absent, not merely emptied by a mask. Never `/` or the sandbox's `$HOME`.
    let real_c = real_home
        .filter(|r| r.is_absolute() && *r != Path::new("/") && *r != home && **r != *home_c)
        .map(canon)
        .filter(|r| r != &home_c && r.parent().is_some() && std::fs::metadata(r).is_ok_and(|m| m.is_dir()));
    // #188: parent first. A later tmpfs over an ancestor hides an earlier one below it, and
    // the final `--remount-ro` of the hidden child then fails; so the outer home goes first.
    let home_first = real_c.as_ref().is_none_or(|r| !home_c.starts_with(r));
    if home_first {
        push(&mut a, &["--tmpfs", &home_c.to_string_lossy()]);
    }
    if let Some(r) = &real_c {
        push(&mut a, &["--tmpfs", &r.to_string_lossy()]);
    }
    if !home_first {
        push(&mut a, &["--tmpfs", &home_c.to_string_lossy()]);
    }
    let mode = effective_mode(s, home, layout, real_home);
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
    let hooks = configured_hook_paths(cwd_path, &home_c, layout);
    let mounts = hooks_mounts(cwd_path, &home_c, layout, mode);
    // Pins and layout mounts share parent-first order: a cache bind cannot hide a pin,
    // and a pin cannot hide a read-only descendant. The cwd is already a mount point.
    let hook_pins = hook_parent_pins(&hooks, &mounts);
    // B2: toolchains read-only, caches read-write, in path order (a parent before its child).
    let mut binds: Vec<(&Path, bool)> = layout
        .read
        .iter()
        .map(|p| (p.as_path(), false))
        .chain(layout.write.iter().map(|p| (p.as_path(), true)))
        .chain(hook_pins.iter().map(|p| (p.as_path(), true)))
        .filter(|(p, _)| !cwd_path.starts_with(p) && !cwd_path.starts_with(canon(p)))
        .collect();
    binds.sort();
    binds.dedup();
    for (p, rw) in &binds {
        let p = p.to_string_lossy();
        push(&mut a, &[if *rw { "--bind" } else { "--ro-bind" }, &p, &p]);
    }
    // #52: with network, `gh` may read its benign config (read-only, only the sandbox's own
    // `$HOME`; offline it stays masked like the rest of the deny-list). The token store
    // `hosts.yml` is never mounted (empty over it): the one token comes in as `GH_TOKEN`, so
    // the sandbox never sees the other hosts' tokens. The directory is bound by its canonical
    // path, and refused when it resolves into (or over) anything on the deny-list of either
    // home, or when `hosts.yml` is a symlink.
    let gh = home_c.join(GH_CONFIG);
    let gh_src = canon(&gh);
    let gh_empty = if s.net { private_empty_file() } else { None };
    let mut gh_masks: Vec<PathBuf> = Vec::new();
    let gh_open = gh_empty.is_some()
        && s.net
        && std::fs::metadata(&gh).is_ok_and(|m| m.is_dir())
        && gh_source_is_safe(&gh_src, &[home, &home_c], real_home, real_c.as_deref(), layout)
        && !std::fs::symlink_metadata(gh_src.join(GH_HOSTS)).is_ok_and(|m| m.file_type().is_symlink());
    if gh_open {
        // An empty, readable regular file (gh fails on `/dev/null` here: permission denied).
        if let (true, Some(_)) = (gh_src.join(GH_HOSTS).exists(), &gh_empty) {
            // The empty-file masks are emitted late (after every bind that could cover them,
            // the git and `.git` binds included), see `gh_masks` below.
            gh_masks.push(gh.join(GH_HOSTS));
            // The same token store at every other visible path of the source (a symlinked
            // config dir inside the cwd, a home workspace entry, another bind, or a place the
            // read-only root shows): the empty file goes over each, after the bind showing it.
            let mut aliases: Vec<PathBuf> = Vec::new();
            if gh_src != gh {
                let hidden_home = |p: &Path| {
                    p.starts_with(home)
                        || p.starts_with(&home_c)
                        || real_c.as_ref().is_some_and(|r| p.starts_with(r))
                };
                // A source under the hidden home is still shown through the cwd bind (an
                // ordinary project directory inside `$HOME`): mask it there as well.
                if !hidden_home(&gh_src) || gh_src.starts_with(&cwd_c) || gh_src.starts_with(cwd_path) {
                    aliases.push(gh_src.clone());
                }
                let via = home_entries
                    .iter()
                    .map(|p| (p.clone(), canon(p)))
                    .chain(binds.iter().map(|(b, _)| (b.to_path_buf(), canon(b))));
                for (dest, src) in via {
                    if let Ok(rest) = gh_src.strip_prefix(&src) {
                        aliases.push(dest.join(rest));
                    }
                }
                aliases.sort();
                aliases.dedup();
                aliases.retain(|p| *p != gh);
            }
            for al in aliases {
                gh_masks.push(al.join(GH_HOSTS));
            }
        }
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
    // Pin the git directory after ancestor/layout binds; unsafe git data stays read-only.
    if writable {
        let dotgit = cwd_path.join(".git");
        if is_real_dir(&dotgit) {
            let p = dotgit.to_string_lossy();
            let bind = if unsafe_reason(&dotgit).is_none() {
                "--bind"
            } else {
                "--ro-bind"
            };
            push(&mut a, &[bind, &p, &p]);
        }
    }
    // This read-only configuration bind must also win over every writable ancestor pin.
    if gh_open {
        push(
            &mut a,
            &["--ro-bind", &gh_src.to_string_lossy(), &gh.to_string_lossy()],
        );
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
        // The directory was pinned before all masks; unsafe git data stays read-only.
        let dotgit_writable = is_real_dir(&dotgit) && unsafe_reason(&dotgit).is_none();
        let protected = protected_paths(cwd_path);
        for p in &protected {
            let config = p.ends_with(".oxa") || p.ends_with(".agents/skills");
            if layout.allow_project_config_write && config && is_plain_git_update(&s.command) {
                continue;
            }
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
                push(&mut a, &["--tmpfs", &ps]);
                if s.net && p == Path::new("/run/systemd/resolve") {
                    // Preserve only DNS configuration online; all resolver IPC stays hidden.
                    for name in ["resolv.conf", "stub-resolv.conf"] {
                        let file = p.join(name);
                        if std::fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
                            let file = file.to_string_lossy();
                            push(&mut a, &["--ro-bind", &file, &file]);
                        }
                    }
                }
                push(&mut a, &["--remount-ro", &ps]);
            } else {
                push(&mut a, &["--ro-bind", "/dev/null", &ps]);
            }
        }
    }
    // #60: the gh token-store masks, after every bind that could cover them (the git data and the
    // cwd's `.git` above included), like the deny-list below.
    if let Some(e) = &gh_empty {
        for m in &gh_masks {
            push(&mut a, &["--ro-bind", &e.to_string_lossy(), &m.to_string_lossy()]);
        }
    }
    // B3: the deny-list, after every bind so no bind re-exposes it. sbx-symhome: each entry is
    // masked where it is shown: at its own path (as before), at its canonical path when that
    // lies outside the hidden home, and inside every mount whose canonical source holds it
    // (the cwd, the home workspace's entries, the layout's binds at their own destinations,
    // the git data), so a symlink in the way never leaves a second, unmasked route.
    let shown = |p: &Path| {
        (!p.starts_with(home)
            && !p.starts_with(&home_c)
            && !real_c.as_ref().is_some_and(|r| p.starts_with(r)))
            || p.starts_with(cwd_path)
            || binds.iter().any(|(b, _)| p.starts_with(b))
            || layout.git.paths().any(|b| p.starts_with(b))
    };
    let mut mounts: Vec<(PathBuf, PathBuf)> = vec![(cwd_c.clone(), cwd_c.clone())];
    mounts.extend(home_entries.iter().map(|p| (p.clone(), canon(p))));
    mounts.extend(binds.iter().map(|(b, _)| (b.to_path_buf(), canon(b))));
    mounts.extend(layout.git.paths().map(|b| (b.to_path_buf(), canon(b))));
    let mut masks: Vec<(PathBuf, bool)> = Vec::new();
    // #53: the deny-list of `$HOME` and of the account's real home, so a changed `$HOME`
    // never widens access.
    let mut homes: Vec<&Path> = vec![home];
    homes.extend(real_home.filter(|r| *r != home));
    let mut deny: Vec<PathBuf> = homes
        .iter()
        .flat_map(|h| sensitive_paths(h, layout.agent_dir.as_deref()))
        .collect();
    deny.sort();
    deny.dedup();
    for p in deny {
        if gh_open && (p == home.join(GH_CONFIG) || p == gh) {
            continue;
        }
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
        if !pc.starts_with(&home_c)
            && !pc.starts_with(home)
            && !real_c.as_ref().is_some_and(|r| pc.starts_with(r))
        {
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
    // Mount points below the hidden home must be created before sealing its root.
    // Child workspace/cache binds remain writable; unbound home writes now fail with EROFS.
    // A cwd that is a strict ancestor of home (`/`, `/home`) is bound read-only above and covers
    // the home tmpfs: home is then no mount of its own (bwrap's remount fails with EINVAL) and
    // is read-only already. Nothing is bound over home here: a late bind of the host home would
    // cover the masks above and expose the credentials.
    // #188: the account home tmpfs is sealed too (a write to it fails), unless the cwd covers it.
    for h in std::iter::once(&home_c).chain(real_c.as_ref()) {
        if cwd_path.starts_with(h) || !h.starts_with(cwd_path) {
            push(&mut a, &["--remount-ro", &h.to_string_lossy()]);
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

/// The home of the current uid from the passwd database, never from `$HOME`.
pub fn account_home() -> Option<PathBuf> {
    real_home()
}

/// `argv` without the `--tmpfs <account home>` and `--remount-ro <account home>` pairs that hides the real home (for goldens that
/// pin a fake `$HOME` and must not depend on the machine they run on).
pub fn without_account_home(mut argv: Vec<String>) -> Vec<String> {
    if let Some(r) = real_home().map(|r| canon(&r).to_string_lossy().into_owned()) {
        if let Some(i) = argv.windows(2).position(|w| w[0] == "--tmpfs" && w[1] == r) {
            argv.drain(i..i + 2);
        }
        if let Some(i) = argv.windows(2).position(|w| w[0] == "--remount-ro" && w[1] == r) {
            argv.drain(i..i + 2);
        }
    }
    argv
}

/// The home of the current uid from the passwd database, never from `$HOME`.
fn real_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0u8; 16384];
    // SAFETY: getpwuid_r writes only into `pwd` and `buf`, both live and sized as passed.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut res: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(
            libc::getuid(),
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut res,
        );
        if rc != 0 || res.is_null() || pwd.pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr(pwd.pw_dir).to_bytes();
        (!dir.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(dir)))
    }
}

/// The GitHub CLI's config directory under the home, open (read-only) in a networked sandbox.
const GH_CONFIG: &str = ".config/gh";
/// An empty regular file (mode 0444) in a private directory of ours (`$XDG_RUNTIME_DIR` or the
/// temp dir, `oxa-empty-<uid>`, mode 0700), created on first use. `None` when the directory or
/// file is not exactly that (not ours, a symlink, not empty): fail closed.
fn private_empty_file() -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("oxa-empty-{uid}"));
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    let dm = std::fs::symlink_metadata(&dir).ok()?;
    if !dm.is_dir() || dm.uid() != uid || dm.permissions().mode() & 0o077 != 0 {
        return None;
    }
    let f = dir.join("empty");
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o444)
        .open(&f);
    let fm = std::fs::symlink_metadata(&f).ok()?;
    (fm.is_file() && fm.uid() == uid && fm.len() == 0 && fm.permissions().mode() & 0o222 == 0).then_some(f)
}

/// The file in it that holds the tokens of every host.
const GH_HOSTS: &str = "hosts.yml";

/// `src` (the canonical gh config directory) is not on, inside or above any deny-list entry of
/// the sandbox's home(s) and the account's real home (the sandbox home's own `gh` entry apart).
fn gh_source_is_safe(
    src: &Path,
    own_homes: &[&Path],
    real_home: Option<&Path>,
    real_c: Option<&Path>,
    layout: &Layout,
) -> bool {
    let mut homes: Vec<&Path> = own_homes.to_vec();
    homes.extend(real_home);
    homes.extend(real_c);
    homes.iter().all(|h| {
        let own = own_homes.contains(h);
        sensitive_paths(h, layout.agent_dir.as_deref()).iter().all(|d| {
            if own && *d == h.join(GH_CONFIG) {
                return true;
            }
            let dc = canon(d);
            !(src.starts_with(d) || src.starts_with(&dc) || d.starts_with(src) || dc.starts_with(src))
        })
    })
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

/// The protected entries that are directories (`hooks`, `info`): a tmpfs masks them when missing.
fn is_dir_entry(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n == "hooks" || n == "info")
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
    if is_dir_entry(p) {
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
    create_missing_git_files_homes(s, home, layout, real_home().as_deref());
}

/// [`create_missing_git_files`] with the real account home given explicitly (tests).
pub(crate) fn create_missing_git_files_homes(
    s: &ExecSpec,
    home: &Path,
    layout: &Layout,
    real_home: Option<&Path>,
) {
    let cwd_c = canon(&s.cwd);
    let cwd = cwd_c.as_path();
    if effective_mode(s, home, layout, real_home) != CwdMode::Writable {
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
        if is_dir_entry(&p) || !p.parent().is_some_and(is_real_dir) {
            continue;
        }
        // #201: git dies on an empty `commondir`; "." names the git directory itself, which
        // is what a missing one means.
        let body: &[u8] = if p.file_name().is_some_and(|n| n == "commondir") {
            b".\n"
        } else {
            b""
        };
        if let Ok(mut f) = std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
            use std::io::Write as _;
            let _ = f.write_all(body);
        }
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
    let mode = effective_mode(s, home, layout, real_home().as_deref());
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
    let paths = configured_hook_paths(&cwd, &home_c, layout);
    let mut mounts = hooks_mounts(&cwd, &home_c, layout, mode);
    for p in &paths {
        if p.to_str().is_none() {
            return Some("sandbox refused: the repository's core.hooksPath is not valid UTF-8 and cannot be represented in the sandbox argv; commands are not run here".into());
        }
        if let Some(root) = under_any(p, &mounts.rw) {
            let symlink = std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
            if symlink || has_symlink_ancestor(p, root) {
                return Some(format!(
                    "sandbox refused: the repository's core.hooksPath {} is a symlink or lies below one, so its hooks cannot be pinned; commands are not run here",
                    p.display()
                ));
            }
            if std::fs::symlink_metadata(p).is_err() && !p.parent().is_some_and(is_real_dir) {
                return Some(format!(
                    "sandbox refused: the repository's core.hooksPath {} does not exist and its parent is not a real directory, so it cannot be masked; commands are not run here",
                    p.display()
                ));
            }
        } else if hook_path_exposed(p, &mounts) && canon(p) != *p {
            return Some(format!("sandbox refused: the repository's core.hooksPath {} reaches a writable bind through an alias, so it cannot be pinned; commands are not run here", p.display()));
        }
    }
    let masked: Vec<PathBuf> = paths
        .iter()
        .filter(|p| hook_path_exposed(p, &mounts))
        .cloned()
        .collect();
    mounts.rw.extend(hook_parent_pins(&paths, &mounts));
    // These directory masks are emitted after every writable bind. Their bind mounts hide
    // earlier nested mounts, so none of those nested writable roots remains exposed.
    mounts
        .rw
        .retain(|rw| !masked.iter().any(|p| rw != p && rw.starts_with(p)));
    mounts.ro.extend(masked);
    for p in paths {
        if let Some(why) = hooks_alias_refusal(&p, &mounts) {
            return Some(format!(
                "sandbox refused: the repository's core.hooksPath: {why}; commands are not run here"
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

/// `p` is inside one of `roots`, lexically or after resolving the existing part.
fn under_any<'a>(p: &Path, roots: &'a [PathBuf]) -> Option<&'a PathBuf> {
    roots.iter().find(|r| p.starts_with(r) || canon(p).starts_with(r))
}

/// A symlink between `root` and `p`: a mount on `p` would not pin a directory the command can
/// rename (the target of the link is what git opens).
fn has_symlink_ancestor(p: &Path, root: &Path) -> bool {
    let Ok(rel) = p.strip_prefix(root) else {
        // under_any found the canonical target inside a writable root: the spelling reaches
        // it through an alias outside that root, which cannot be pinned component by component.
        return true;
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
    let mounts = hooks_mounts(cwd, home, layout, mode);
    let protected = protected_paths(cwd);
    for p in configured_hook_paths(cwd, home, layout) {
        if !hook_path_exposed(&p, &mounts) {
            continue;
        }
        if (mode == CwdMode::Writable && protected.contains(&p) && is_real(&p))
            || under_any(&p, &mounts.rw).is_some_and(|root| has_symlink_ancestor(&p, root))
        {
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

/// A hooks tree must be masked if writable itself or if it contains an explicit write bind.
fn hook_path_exposed(path: &Path, mounts: &Mounts) -> bool {
    under_any(path, &mounts.rw).is_some() || mounts.rw.iter().any(|rw| rw.starts_with(canon(path)))
}

/// The mount view used by both configured-hook checks and their ancestor pins.
fn hooks_mounts(cwd: &Path, home: &Path, layout: &Layout, mode: CwdMode) -> Mounts {
    let writable = mode == CwdMode::Writable;
    let git_writable = writable && layout.git.rw.iter().all(|d| unsafe_reason(d).is_none());
    let mut mounts = Mounts {
        rw: writable_roots(cwd, home, layout, mode),
        ro: layout.read.iter().map(|p| canon(p)).collect(),
    };
    for p in layout
        .git
        .rw
        .iter()
        .filter(|p| !p.starts_with(cwd) && !cwd.starts_with(p))
    {
        if git_writable {
            mounts.rw.push(p.clone());
        } else {
            mounts.ro.push(p.clone());
        }
    }
    mounts.ro.extend(layout.git.ro.iter().cloned());
    mounts.ro.extend(
        layout
            .git
            .protect
            .iter()
            .filter(|p| !p.starts_with(cwd) && !cwd.starts_with(p))
            .cloned(),
    );
    if writable {
        let dotgit = cwd.join(".git");
        if is_real_dir(&dotgit) {
            if unsafe_reason(&dotgit).is_none() {
                mounts.rw.push(dotgit);
            } else {
                mounts.ro.push(dotgit);
            }
        }
        mounts
            .ro
            .extend(protected_paths(cwd).into_iter().filter(|p| is_real(p)));
    }
    // GH config is read-only online and masked offline. Its late mount hides earlier
    // writable binds below it; source aliases elsewhere keep their own mount permissions.
    let gh = home.join(GH_CONFIG);
    mounts.rw.retain(|p| !p.starts_with(&gh));
    mounts.ro.push(gh);
    mounts
}

/// Writable directories on the way to a hook mount must themselves be mount points.
/// Self-binds retain ordinary writes but prevent moving a protected tree through an ancestor.
fn hook_parent_pins(paths: &[PathBuf], mounts: &Mounts) -> Vec<PathBuf> {
    let mut pins = std::collections::BTreeSet::new();
    for path in paths {
        for parent in path.ancestors().skip(1) {
            if mounts.writable(parent, false) && is_real_dir(parent) {
                pins.insert(parent.to_path_buf());
            }
        }
    }
    pins.into_iter().collect()
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
    if line.is_empty() || line == "." {
        // Main creates this protected self-reference when masking a missing commondir.
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
/// [`cwd_mode`], but a cwd at or above the account's real home (under another `$HOME`) is the
/// home root of that account: never writable as a whole (its startup files would be).
pub(crate) fn effective_mode(
    s: &ExecSpec,
    home: &Path,
    layout: &Layout,
    real_home: Option<&Path>,
) -> CwdMode {
    let home_c = canon(home);
    let cwd_c = canon(&s.cwd);
    let at_real_home = real_home
        .filter(|r| r.is_absolute() && *r != Path::new("/") && *r != home && **r != *home_c)
        .map(canon)
        .filter(|r| r != &home_c && r.parent().is_some() && std::fs::metadata(r).is_ok_and(|m| m.is_dir()))
        .is_some_and(|r| is_home_or_above(&cwd_c, &r) && !is_home_or_above(&cwd_c, &home_c));
    match cwd_mode(s, home, layout) {
        CwdMode::Writable | CwdMode::HomeWorkspace if at_real_home => CwdMode::ReadOnly,
        m => m,
    }
}

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
    use oxa_fake::TestEnv;

    fn spec(cwd: &Path, net: bool) -> ExecSpec {
        ExecSpec {
            command: "true".into(),
            cwd: cwd.to_path_buf(),
            writable: true,
            net,
            timeout_s: None,
            env: Vec::new(),
        }
    }

    /// `--tmpfs P` or `--ro-bind /dev/null P` masks `P`.
    fn masked(a: &[String], p: &Path) -> bool {
        let p = p.to_string_lossy();
        a.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == p)
            || a.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1] == "/dev/null" && w[2] == p)
    }

    fn pos(a: &[String], flag: &str, p: &Path) -> Option<usize> {
        let p = p.to_string_lossy();
        a.windows(2).position(|w| w[0] == flag && w[1] == p)
    }

    /// #188: the home tmpfs mounts go parent first, whatever the relation of `$HOME` and the
    /// account home; every mask comes after every bind and tmpfs.
    #[test]
    fn home_tmpfs_mounts_are_parent_first_in_every_relation() {
        let env = TestEnv::new();
        let root = env.root().join("rel");
        let (real_dir, nested_home) = (root.join("acct"), root.join("acct/priv/home"));
        let work = root.join("acct/priv/work");
        for d in [
            &nested_home,
            &work,
            &root.join("outside/home"),
            &root.join("up/acct"),
        ] {
            std::fs::create_dir_all(d).unwrap();
        }
        for (home, real, label) in [
            (nested_home.clone(), real_dir.clone(), "home below account"),
            (real_dir.clone(), nested_home.clone(), "account below home"),
            (root.join("outside/home"), real_dir.clone(), "unrelated"),
        ] {
            let sp = spec(&work, true);
            let a = bwrap_args_homes(&sp, &home, &Layout::default(), Some(&real));
            let (hc, rc) = (canon(&home), canon(&real));
            let h = pos(&a, "--tmpfs", &hc).unwrap_or_else(|| panic!("{label}: {a:?}"));
            let r = pos(&a, "--tmpfs", &rc).unwrap_or_else(|| panic!("{label}: {a:?}"));
            let (first, second) = if hc.starts_with(&rc) { (r, h) } else { (h, r) };
            if hc.starts_with(&rc) || rc.starts_with(&hc) {
                assert!(first < second, "{label}: {a:?}");
            }
            let bind = pos(&a, "--bind", &canon(&work)).unwrap();
            assert!(h < bind && r < bind, "{label}: {a:?}");
            // The homes stay read-only: each tmpfs is remounted after both are in place.
            for p in [&hc, &rc] {
                let ro = pos(&a, "--remount-ro", p).unwrap_or_else(|| panic!("{label}: {a:?}"));
                assert!(ro > h && ro > r, "{label}: {a:?}");
            }
        }
        // Masks come after all home tmpfs and binds.
        env.write("rel/acct/priv/home/.ssh/id", "K");
        env.write("rel/acct/.ssh/id", "K");
        let a = bwrap_args_homes(
            &spec(&work, true),
            &nested_home,
            &Layout::default(),
            Some(&real_dir),
        );
        let last_mount = a
            .iter()
            .rposition(|x| x == "--bind")
            .unwrap()
            .max(pos(&a, "--tmpfs", &canon(&real_dir)).unwrap());
        for m in [nested_home.join(".ssh"), real_dir.join(".ssh")] {
            if let Some(i) = pos(&a, "--tmpfs", &m) {
                assert!(i > last_mount, "{m:?}: {a:?}");
            }
        }
    }

    /// #188, real bwrap: `$HOME` below the account home.
    #[test]
    fn nested_home_below_the_account_home_runs_and_stays_sealed() {
        if std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "true"])
            .status()
            .map_or(true, |s| !s.success())
        {
            assert!(std::env::var("OXA_REQUIRE_BWRAP").map_or(true, |v| v != "1"));
            return;
        }
        let env = TestEnv::new();
        let real = env.root().join("acct188");
        let home = real.join("priv/home");
        let work = real.join("priv/work");
        env.write("acct188/priv/work/ok.txt", "WORKSPACE_OK");
        env.write("acct188/priv/home/.ssh/id", "PRIVATE_NESTED");
        env.write("acct188/.netrc", "PRIVATE_REAL");
        let run = |cmd: &str| {
            let mut sp = spec(&work, false);
            sp.command = cmd.into();
            let mut argv = bwrap_args_homes(&sp, &home, &Layout::default(), Some(&real));
            argv.retain(|x| x != "--die-with-parent");
            let o = std::process::Command::new("bwrap").args(&argv).output().unwrap();
            (
                o.status.code(),
                String::from_utf8_lossy(&[o.stdout, o.stderr].concat()).into_owned(),
            )
        };
        let ok = run("cat ok.txt");
        assert_eq!(ok.0, Some(0), "{}", ok.1);
        assert!(ok.1.contains("WORKSPACE_OK"), "{}", ok.1);
        let creds = run(&format!(
            "cat '{0}/.ssh/id' '{1}/.netrc' 2>&1; true",
            home.display(),
            real.display()
        ));
        assert!(!creds.1.contains("PRIVATE"), "{}", creds.1);
        for d in [&home, &real] {
            let w = run(&format!("touch '{}/x'", d.display()));
            assert_ne!(w.0, Some(0), "{}: {}", d.display(), w.1);
        }
    }

    #[test]
    fn a_different_home_hides_the_real_home_in_every_mode() {
        let env = TestEnv::new();
        let real = env.root().join("realhome");
        env.write("realhome/.ssh/id", "K");
        env.write("realhome/.config/oxc/llm/cloud-secrets.json", "{}");
        let real_s = real.to_string_lossy().into_owned();
        for (writable, net) in [(true, true), (true, false), (false, true), (false, false)] {
            let mut sp = spec(&env.work(), net);
            sp.writable = writable;
            let a = bwrap_args_homes(&sp, &env.home(), &Layout::default(), Some(&real));
            // An empty tmpfs over the real home: `~/.ssh` and the cloud keys are absent
            // (`test -r` fails), not just emptied by a mask.
            assert!(a.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == real_s), "{a:?}");
        }
    }

    #[test]
    fn a_cwd_inside_the_real_home_keeps_its_credentials_masked() {
        let env = TestEnv::new();
        let real = env.root().join("realhome");
        env.write("realhome/proj/x", "1");
        env.write("realhome/proj/.ssh/id", "K");
        let cwd = real.join("proj");
        // A credential path that lies inside the cwd mount is masked there.
        let layout = Layout {
            agent_dir: Some(cwd.join(".ssh")),
            ..Layout::default()
        };
        let a = bwrap_args_homes(&spec(&cwd, true), &env.home(), &layout, Some(&real));
        assert!(masked(&a, &cwd.join(".ssh")), "{a:?}");
        let real_s = real.to_string_lossy().into_owned();
        let tmpfs = a.iter().position(|x| *x == real_s).expect("real home hidden");
        let bind = a
            .iter()
            .position(|x| *x == cwd.to_string_lossy())
            .expect("cwd bound");
        assert!(tmpfs < bind, "{a:?}");
    }

    #[test]
    fn gh_config_is_readable_online_and_hidden_offline() {
        let env = TestEnv::new();
        let gh = env.home().join(".config/gh");
        env.write("home/.config/gh/hosts.yml", "x");
        let on = bwrap_args_homes(&spec(&env.work(), true), &env.home(), &Layout::default(), None);
        assert!(on.iter().any(|x| x == "--share-net"));
        assert!(!masked(&on, &gh), "{on:?}");
        let ghs = gh.to_string_lossy().into_owned();
        assert!(
            on.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1] == ghs && w[2] == ghs),
            "{on:?}"
        );
        let off = bwrap_args_homes(&spec(&env.work(), false), &env.home(), &Layout::default(), None);
        // Offline the home is an empty tmpfs: gh's directory is not bound in.
        assert!(!off.contains(&ghs), "{off:?}");
    }

    #[test]
    fn a_cwd_at_the_real_home_is_not_writable_under_another_home() {
        let env = TestEnv::new();
        let real = env.root().join("realhome");
        env.write("realhome/.bashrc", "# rc");
        env.write("realhome/proj/x", "1");
        let real_c = canon(&real).to_string_lossy().into_owned();
        let mut sp = spec(&real, false);
        sp.writable = true;
        let a = bwrap_args_homes(&sp, &env.home(), &Layout::default(), Some(&real));
        assert!(
            !a.windows(3)
                .any(|w| w[0] == "--bind" && w[1] == real_c && w[2] == real_c),
            "the real home is bound writable: {a:?}"
        );
    }

    #[test]
    fn no_git_files_are_created_in_a_cwd_at_the_real_home() {
        let env = TestEnv::new();
        let real = env.root().join("realhome");
        env.write("realhome/.git/HEAD", "ref: refs/heads/main\n");
        let mut sp = spec(&real, false);
        sp.writable = true;
        create_missing_git_files_homes(&sp, &env.home(), &Layout::default(), Some(&real));
        assert!(!real.join(".git/config").exists());
        // Control: a normal project still gets its empty config.
        env.write("work/.git/HEAD", "ref: refs/heads/main\n");
        let mut sp = spec(&env.work(), false);
        sp.writable = true;
        create_missing_git_files_homes(&sp, &env.home(), &Layout::default(), Some(&real));
        assert!(env.work().join(".git/config").exists());
    }

    fn binds_src(a: &[String], p: &Path) -> bool {
        let p = p.to_string_lossy();
        a.windows(3).any(|w| {
            (w[0] == "--ro-bind" || w[0] == "--bind") && w[1] != "/dev/null" && (w[1] == p || w[2] == p)
        })
    }

    #[test]
    fn a_symlinked_gh_config_to_a_protected_dir_is_not_exposed() {
        let env = TestEnv::new();
        env.write("home/.ssh/id_synth", "SYNTH");
        std::fs::create_dir_all(env.home().join(".config")).unwrap();
        let gh = env.home().join(".config/gh");
        std::os::unix::fs::symlink(env.home().join(".ssh"), &gh).unwrap();
        let on = bwrap_args_homes(&spec(&env.work(), true), &env.home(), &Layout::default(), None);
        assert!(!binds_src(&on, &gh), "{on:?}");
        assert!(!binds_src(&on, &env.home().join(".ssh")), "{on:?}");
        // Also in the real account home (another `$HOME`).
        let real = env.root().join("realhome");
        env.write("realhome/.aws/credentials", "SYNTH");
        std::fs::create_dir_all(real.join(".config")).unwrap();
        std::os::unix::fs::symlink(real.join(".aws"), real.join(".config/gh")).unwrap();
        env.write("home/.config/gh2/x", "");
        let _ = std::fs::remove_file(&gh);
        std::os::unix::fs::symlink(real.join(".aws"), &gh).unwrap();
        let on = bwrap_args_homes(
            &spec(&env.work(), true),
            &env.home(),
            &Layout::default(),
            Some(&real),
        );
        assert!(!binds_src(&on, &gh), "{on:?}");
    }

    #[test]
    fn gh_token_store_is_masked_but_benign_config_stays_online() {
        let env = TestEnv::new();
        env.write("home/.config/gh/hosts.yml", "SYNTH-OTHER-HOST-TOKEN");
        env.write("home/.config/gh/config.yml", "editor: vi");
        let gh = env.home().join(".config/gh");
        let on = bwrap_args_homes(&spec(&env.work(), true), &env.home(), &Layout::default(), None);
        assert!(binds_src(&on, &gh), "{on:?}");
        let hy = gh.join("hosts.yml").to_string_lossy().into_owned();
        assert!(
            on.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1].ends_with("/empty") && w[2] == hy),
            "{on:?}"
        );
        assert!(!masked(&on, &gh.join("hosts.yml")), "never /dev/null: {on:?}");
        let ghs = gh.to_string_lossy().into_owned();
        let bind = on.iter().position(|x| *x == ghs).unwrap();
        let mask = on
            .iter()
            .position(|x| *x == gh.join("hosts.yml").to_string_lossy())
            .unwrap();
        assert!(bind < mask, "mask must come after the bind: {on:?}");
        let off = bwrap_args_homes(&spec(&env.work(), false), &env.home(), &Layout::default(), None);
        assert!(!binds_src(&off, &gh), "{off:?}");
    }

    /// #122: starting in the home directory keeps the credential masks.
    #[test]
    fn credential_masks_hold_with_cwd_home() {
        let env = TestEnv::new();
        env.write("home/.config/gh/hosts.yml", "SYNTH-TOKEN");
        env.write("home/.ssh/id_test", "SYNTH");
        let home = env.home();
        let on = bwrap_args_homes(&spec(&home, true), &home, &Layout::default(), None);
        let hy = home.join(".config/gh/hosts.yml").to_string_lossy().into_owned();
        assert!(
            on.windows(3)
                .any(|w| w[0] == "--ro-bind" && w[1].ends_with("/empty") && w[2] == hy),
            "{on:?}"
        );
        let ssh = home.join(".ssh").to_string_lossy().into_owned();
        assert!(
            !on.iter()
                .any(|x| x == &home.join(".ssh/id_test").to_string_lossy()),
            "{on:?}"
        );
        assert!(!on.windows(3).any(|w| w[0] == "--bind" && w[1] == ssh), "{on:?}");
    }

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
