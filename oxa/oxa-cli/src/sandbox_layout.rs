//! B2 / B3 (wave H lane `harden`): the bash sandbox's view of the machine, from the user's
//! environment and the USER settings only (a project file cannot widen the sandbox).
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::Level;
use oxa_sandbox::{
    expand_home, is_home_itself, is_home_or_above, parse_scratch_warn, Layout, LayoutInputs,
    DEFAULT_SCRATCH_WARN, SETTING_ALLOW_HOME_WRITE, SETTING_READ_PATHS, SETTING_SOCKETS, SETTING_WRITE_PATHS,
};
use std::path::{Path, PathBuf};

/// What stays protected in a home workspace (O-199), in the user's words.
const HOME_PROTECTED_SUMMARY: &str = "the home folder itself, every dotfile and dot-folder (.bashrc, .profile, .config, .local/bin, .ssh, .oxa, ...) and ~/bin are read-only; keys and tokens are hidden; bash cannot create new top-level folders in ~, so create one with the file tools or outside oxa";

/// The startup warning when oxa runs in `$HOME`, `/` or a parent of `$HOME` without a home
/// workspace (R6 shape).
pub(crate) fn home_root_warning(cwd: &Path, home_itself: bool) -> String {
    if home_itself {
        format!(
            "Bash has no writable directory: oxa was started in {}, which is your home directory\nFix: start oxa in a project directory, or set \"{SETTING_ALLOW_HOME_WRITE}\": true in the user settings.json",
            cwd.display()
        )
    } else {
        format!(
            "Bash has no writable directory: oxa was started in {}, which is a parent of your home directory or /\nFix: start oxa in a project directory",
            cwd.display()
        )
    }
}

/// The startup notice of a home workspace (O-199).
pub(crate) fn home_workspace_notice() -> String {
    format!("workspace = your home directory: bash can write in its folders and files (protected: {HOME_PROTECTED_SUMMARY})")
}

/// The extra line of the interactive start question in `$HOME` (`home_cwd`).
pub(crate) fn home_confirm_effect() -> String {
    format!("If you go on, your home directory is the workspace: bash can write in its folders and files (protected: {HOME_PROTECTED_SUMMARY}).")
}

/// The layout for this process; pushes the B3 startup warning or the O-199 home workspace
/// notice when they apply. `confirmed` = the interactive start in `$HOME` itself was confirmed
/// (`home_cwd::check`); the user setting [`SETTING_ALLOW_HOME_WRITE`] does the same in every
/// mode. Neither ever makes `/` or a parent of `$HOME` writable.
/// True when the binary lives under a writable root (workspace or a writable extra path):
/// a read-only bind there would make `cargo build`/`clean` fail with EBUSY.
fn exe_under_writable(exe: &Path, roots: &[&Path]) -> bool {
    let canon = exe.canonicalize().ok();
    roots.iter().any(|r| {
        let rc = r.canonicalize().ok();
        exe.starts_with(r)
            || canon.as_deref().is_some_and(|k| k.starts_with(r))
            || rc
                .as_deref()
                .is_some_and(|rc| exe.starts_with(rc) || canon.as_deref().is_some_and(|k| k.starts_with(rc)))
    })
}

#[cfg(test)]
mod exe_tests {
    use super::*;

    #[test]
    fn exe_under_cwd_or_writable_extra_is_detected() {
        let d = std::env::temp_dir().join(format!("oxa-exe-{}", std::process::id()));
        let (w, other) = (d.join("w"), d.join("o"));
        std::fs::create_dir_all(w.join("target")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let exe = w.join("target/oxa");
        std::fs::write(&exe, b"").unwrap();
        assert!(exe_under_writable(&exe, &[other.as_path(), w.as_path()]));
        assert!(!exe_under_writable(&exe, &[other.as_path()]));
        std::fs::remove_dir_all(&d).ok();
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;
    use oxa_config::dirs_from_env;
    use oxa_fake::TestEnv;

    fn env_value(l: &Layout, name: &str) -> Option<String> {
        l.env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn an_unattended_session_never_waits_at_a_credential_prompt() {
        let env = TestEnv::new();
        for unattended in [false, true] {
            let mut owned = env.envs("http://127.0.0.1:9");
            if unattended {
                owned.push(("OXA_UNATTENDED".to_string(), "1".to_string()));
            }
            let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let e = Env::from_pairs(&pairs);
            let dirs = dirs_from_env(&e, None).expect("dirs");
            let l = layout(&e, &dirs, &env.work(), false, &mut Vec::new());
            assert_eq!(
                env_value(&l, "GIT_TERMINAL_PROMPT").as_deref(),
                unattended.then_some("0")
            );
            assert_eq!(
                env_value(&l, "GH_PROMPT_DISABLED").as_deref(),
                unattended.then_some("1")
            );
        }
    }
}

/// Expand a bind list from the user-only settings snapshot used to build the sandbox.
pub(crate) fn configured_paths(user: &Settings, key: &str, home: &Path) -> Vec<std::path::PathBuf> {
    user.list(key).iter().map(|raw| expand_home(raw, home)).collect()
}

/// Locations that cannot be trusted to supply the runner's token helper. Reuse the
/// layout's implicit cache rules without creating a cache or mounting anything.
pub(crate) fn worker_token_write_paths(user: &Settings, env: &Env, dirs: &Dirs) -> Vec<PathBuf> {
    let mut writes = configured_paths(user, SETTING_WRITE_PATHS, &dirs.home);
    let implicit = Layout::build(&LayoutInputs {
        home: dirs.home.clone(),
        agent_dir: Some(dirs.agent_dir.clone()),
        env: env.pairs().to_vec(),
        ..LayoutInputs::default()
    });
    writes.extend(implicit.write);
    // Workers from other worktrees or repositories can also write here. Keep the
    // namespace untrusted even if shared caching is currently disabled.
    writes.push(crate::worker_cache::cache_root(user, &dirs.home, env));
    let resolved: Vec<_> = writes
        .iter()
        .filter_map(|path| path.canonicalize().ok())
        .collect();
    writes.extend(resolved);
    writes.sort();
    writes.dedup();
    writes
}

pub(crate) fn layout(
    env: &Env,
    dirs: &Dirs,
    cwd: &Path,
    confirmed: bool,
    notices: &mut Vec<(Level, String)>,
) -> Layout {
    let user = Settings::load(dirs, cwd, false);
    let paths = |key| configured_paths(&user, key, &dirs.home);
    let resolved = cwd.canonicalize().ok();
    let home_root = is_home_or_above(cwd, &dirs.home)
        || resolved
            .as_deref()
            .is_some_and(|r| is_home_or_above(r, &dirs.home));
    let home_itself = is_home_itself(cwd, &dirs.home);
    let allow_home_write = home_itself && (confirmed || user.bool(SETTING_ALLOW_HOME_WRITE, false));
    if home_root && allow_home_write {
        notices.push((Level::Info, home_workspace_notice()));
    } else if home_root {
        notices.push((Level::Warning, home_root_warning(cwd, home_itself)));
    }
    let read_extra = paths(SETTING_READ_PATHS);
    let exe = std::env::current_exe().ok();
    let mut write_extra = paths(SETTING_WRITE_PATHS);
    // #1541: a worker derives its own per-repository build cache: exactly one more writable
    // path and `CARGO_TARGET_DIR` for it. Failure never blocks; the worker builds without it.
    let mut cache_target = None;
    match crate::worker_cache::derive(&user, &dirs.home, env, cwd) {
        crate::worker_cache::Derived::Path(p) => {
            write_extra.push(p.clone());
            cache_target = Some(p);
        }
        crate::worker_cache::Derived::Failed(why) => notices.push((
            Level::Warning,
            format!(
                "worker build cache is off for this run ({why}); builds use the workspace target directory"
            ),
        )),
        crate::worker_cache::Derived::None => {}
    }
    let mut roots: Vec<&Path> = vec![cwd];
    roots.extend(resolved.as_deref());
    roots.extend(write_extra.iter().map(|p| p.as_path()));
    let exe_in_workspace = exe.as_deref().is_some_and(|e| exe_under_writable(e, &roots));
    let mut built = Layout::build(&LayoutInputs {
        home: dirs.home.clone(),
        agent_dir: Some(dirs.agent_dir.clone()),
        path_var: env.get("PATH").unwrap_or("").to_string(),
        env: env.pairs().to_vec(),
        read_extra: read_extra.clone(),
        write_extra,
        allow_home_write,
        exe,
        exe_in_workspace,
    });
    // #196: extra read-only paths are never silent; say which are mounted and which are not.
    // A bind that contains the cwd is never mounted (the cwd bind covers it): its siblings
    // stay hidden and the cwd keeps its own access.
    let holds_cwd = |m: &Path| {
        let canon = m.canonicalize().ok();
        [Some(cwd), resolved.as_deref()]
            .into_iter()
            .flatten()
            .any(|c| c.starts_with(m) || canon.as_deref().is_some_and(|k| c.starts_with(k)))
    };
    let (kept, skipped): (Vec<_>, Vec<_>) = read_extra
        .iter()
        .partition(|p| Layout::keeps_extra(&dirs.home, Some(&dirs.agent_dir), p));
    let (over_cwd, kept): (Vec<_>, Vec<_>) = kept.into_iter().partition(|p| holds_cwd(p));
    let (mounted, unmounted): (Vec<_>, Vec<_>) = kept.into_iter().partition(|p| {
        built
            .read
            .iter()
            .chain(&built.write)
            .any(|m| p.starts_with(m) && !holds_cwd(m))
    });
    let skipped: Vec<_> = skipped.into_iter().chain(unmounted).collect();
    if !over_cwd.is_empty() {
        let list: Vec<_> = over_cwd.iter().map(|p| p.display().to_string()).collect();
        notices.push((
            Level::Info,
            format!(
                "{SETTING_READ_PATHS} contains the workspace, so only the workspace itself is mounted there (siblings stay hidden): {}",
                list.join(", ")
            ),
        ));
    }
    let (writable, rest): (Vec<&&std::path::PathBuf>, Vec<&&std::path::PathBuf>) =
        mounted.iter().partition(|p| {
            // The innermost real mount decides: a read bind nested in a writable one stays
            // read-only, and a writable bind holding the cwd is never mounted.
            built
                .read
                .iter()
                .map(|m| (m, false))
                .chain(built.write.iter().map(|m| (m, true)))
                .filter(|(m, _)| p.starts_with(m) && !holds_cwd(m))
                .max_by_key(|(m, _)| m.components().count())
                .is_some_and(|(_, rw)| rw)
        });
    let (partly, readonly): (Vec<&&std::path::PathBuf>, Vec<&&std::path::PathBuf>) =
        rest.into_iter().partition(|p| built.has_writable_inside(p));
    if !partly.is_empty() {
        let list: Vec<_> = partly.iter().map(|p| p.display().to_string()).collect();
        notices.push((
            Level::Info,
            format!(
                "bash can also read (read-only, except writable paths inside it): {} ({SETTING_READ_PATHS})",
                list.join(", ")
            ),
        ));
    }
    if !readonly.is_empty() {
        let list: Vec<_> = readonly.iter().map(|p| p.display().to_string()).collect();
        notices.push((
            Level::Info,
            format!(
                "bash can also read (never write): {} ({SETTING_READ_PATHS})",
                list.join(", ")
            ),
        ));
    }
    if !writable.is_empty() {
        let list: Vec<_> = writable.iter().map(|p| p.display().to_string()).collect();
        notices.push((
            Level::Info,
            format!(
                "bash can also read and write (also writable by another setting): {} ({SETTING_READ_PATHS})",
                list.join(", ")
            ),
        ));
    }
    if !skipped.is_empty() {
        let list: Vec<_> = skipped.iter().map(|p| p.display().to_string()).collect();
        notices.push((
            Level::Warning,
            format!(
                "{SETTING_READ_PATHS} ignored (missing, hidden credentials or the home itself): {}",
                list.join(", ")
            ),
        ));
    }
    if let Some(target) = cache_target {
        built.env.retain(|(n, _)| n != "CARGO_TARGET_DIR");
        built
            .env
            .push(("CARGO_TARGET_DIR".to_string(), target.display().to_string()));
        built.env.sort();
    }
    built.mask_runtime_ipc = true;
    built.sockets = listed_sockets(&paths(SETTING_SOCKETS), notices);
    built.scratch_warn = match user
        .string(SETTING_SCRATCH_WARN)
        .map(|raw| parse_scratch_warn(&raw))
    {
        None => Some(DEFAULT_SCRATCH_WARN),
        Some(Ok(limit)) => limit,
        Some(Err(why)) => {
            notices.push((Level::Warning, format!("{SETTING_SCRATCH_WARN} ignored: {why}")));
            Some(DEFAULT_SCRATCH_WARN)
        }
    };
    // #639: the git config keys ordinary git operations may change; a user-only list.
    built.git_config_allow = Some(match user.get(SETTING_GIT_CONFIG_ALLOW) {
        Some(_) => user.list(SETTING_GIT_CONFIG_ALLOW),
        None => DEFAULT_GIT_CONFIG_ALLOW
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    });
    // #1543: nobody can answer a credential prompt in an unattended run, so an expired
    // credential fails at once with the 401 the worker reports instead of blocking.
    if env.get("OXA_UNATTENDED").is_some() {
        built
            .env
            .retain(|(n, _)| n != "GIT_TERMINAL_PROMPT" && n != "GH_PROMPT_DISABLED");
        built
            .env
            .push(("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()));
        built
            .env
            .push(("GH_PROMPT_DISABLED".to_string(), "1".to_string()));
        built.env.sort();
    }
    built
}

/// #639: the git config keys a sandboxed command may change (the rest is restored after it).
const SETTING_GIT_CONFIG_ALLOW: &str = "oxa.sandbox.gitConfigAllow";
const DEFAULT_GIT_CONFIG_ALLOW: &[&str] = &["branch.*", "user.*", "remote.*.fetch"];

/// #486: the soft size limit of the session's `/tmp` scratch (a warning, never a block).
const SETTING_SCRATCH_WARN: &str = "oxa.sandbox.scratchWarnSize";

/// RP6: the owner's listed sockets that exist and are Unix sockets, canonical and sorted. Each
/// one is named in a start notice that states the risk; a path that is not a socket is
/// reported and ignored. Runtime binds require network access and a validated descriptor.
fn listed_sockets(
    listed: &[std::path::PathBuf],
    notices: &mut Vec<(Level, String)>,
) -> Vec<std::path::PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    let (ok, bad): (Vec<_>, Vec<_>) = listed.iter().partition(|p| {
        p.is_absolute()
            && p.canonicalize()
                .and_then(std::fs::metadata)
                .is_ok_and(|m| m.file_type().is_socket())
    });
    let mut socks: Vec<_> = ok.iter().filter_map(|p| p.canonicalize().ok()).collect();
    socks.sort();
    socks.dedup();
    if !bad.is_empty() {
        let list: Vec<_> = bad.iter().map(|p| p.display().to_string()).collect();
        notices.push((
            Level::Warning,
            format!(
                "{SETTING_SOCKETS} ignored (not an absolute path to an existing socket): {}",
                list.join(", ")
            ),
        ));
    }
    if !socks.is_empty() {
        notices.push((Level::Warning, socket_notice(&socks)));
    }
    socks
}

/// The start notice of exposed sockets: names each and says what the access means.
pub(crate) fn socket_notice(socks: &[std::path::PathBuf]) -> String {
    let list: Vec<_> = socks.iter().map(|p| p.display().to_string()).collect();
    let runtime = list
        .iter()
        .any(|p| ["docker", "podman", "containerd"].iter().any(|n| p.contains(n)));
    let risk = if runtime {
        "a container runtime socket gives root on this machine"
    } else {
        "whatever the service behind a socket allows, bash can do"
    };
    format!(
        "host sockets configured for network-enabled bash: {} ({SETTING_SOCKETS}); {risk}",
        list.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_config::dirs_from_env;
    use oxa_fake::TestEnv;

    fn setup(env: &TestEnv) -> (Env, Dirs) {
        let owned = env.envs("http://127.0.0.1:9");
        let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let e = Env::from_pairs(&pairs);
        let dirs = dirs_from_env(&e, None).expect("dirs");
        (e, dirs)
    }

    #[test]
    fn scratch_warning_is_optional_and_an_invalid_size_keeps_the_default() {
        let env = TestEnv::new();
        let (e, dirs) = setup(&env);
        for (value, limit, warnings) in [
            (None, Some(20 << 30), 0),
            (Some("512MiB"), Some(512 << 20), 0),
            (Some("off"), None, 0),
            (Some("not a size"), Some(20 << 30), 1),
        ] {
            if let Some(value) = value {
                env.write(
                    "home/.oxa/agent/settings.json",
                    &format!(r#"{{"oxa":{{"sandbox":{{"scratchWarnSize":"{value}"}}}}}}"#),
                );
            }
            let mut notices = Vec::new();
            let built = layout(&e, &dirs, &env.work(), false, &mut notices);
            assert_eq!(built.scratch_warn, limit, "{value:?}");
            assert_eq!(notices.len(), warnings, "{value:?}: {notices:?}");
            assert!(notices.iter().all(|(level, _)| *level == Level::Warning));
        }
    }

    #[test]
    fn a_confirmed_start_in_home_is_a_workspace_with_one_notice() {
        let env = TestEnv::new();
        let (e, dirs) = setup(&env);
        let mut notices = Vec::new();
        let l = layout(&e, &dirs, &env.home(), true, &mut notices);
        assert!(l.allow_home_write);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].0, Level::Info);
        assert!(
            notices[0].1.starts_with("workspace = your home directory"),
            "{notices:?}"
        );
        // Not confirmed, no setting: read-only and the warning.
        let mut notices = Vec::new();
        let l = layout(&e, &dirs, &env.home(), false, &mut notices);
        assert!(!l.allow_home_write);
        assert!(
            notices[0].1.starts_with("Bash has no writable directory"),
            "{notices:?}"
        );
    }

    #[test]
    fn a_parent_of_home_is_never_a_workspace() {
        let env = TestEnv::new();
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"oxa":{"sandbox":{"allowHomeWrite":true}}}"#,
        );
        let (e, dirs) = setup(&env);
        for cwd in [env.root().to_path_buf(), std::path::PathBuf::from("/")] {
            let mut notices = Vec::new();
            let l = layout(&e, &dirs, &cwd, true, &mut notices);
            assert!(!l.allow_home_write, "{cwd:?}");
            assert!(notices
                .iter()
                .any(|(_, t)| t.contains("a parent of your home directory")));
        }
        // A project below home: never a home workspace, no notice.
        let mut notices = Vec::new();
        let l = layout(&e, &dirs, &env.work(), true, &mut notices);
        assert!(!l.allow_home_write);
        assert!(notices.is_empty(), "{notices:?}");
    }

    #[test]
    fn read_path_with_writable_child_is_not_called_never_write() {
        let env = TestEnv::new();
        env.write("data/out/x", "x");
        let r = |p: &str| env.root().join(p).display().to_string();
        env.write(
            "home/.oxa/agent/settings.json",
            &format!(
                r#"{{"oxa":{{"sandbox":{{"readPaths":["{}"],"writePaths":["{}"]}}}}}}"#,
                r("data"),
                r("data/out")
            ),
        );
        let (e, dirs) = setup(&env);
        let mut notices = Vec::new();
        layout(&e, &dirs, &env.work(), false, &mut notices);
        assert!(
            !notices.iter().any(|(_, t)| t.contains("(never write)")),
            "{notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|(_, t)| t.contains("except writable paths inside")),
            "{notices:?}"
        );
    }

    #[test]
    fn read_path_nested_in_a_writable_path_is_still_never_write() {
        let env = TestEnv::new();
        env.write("srv/ref/x", "x");
        let r = |p: &str| env.root().join(p).display().to_string();
        env.write(
            "home/.oxa/agent/settings.json",
            &format!(
                r#"{{"oxa":{{"sandbox":{{"readPaths":["{}"],"writePaths":["{}"]}}}}}}"#,
                r("srv/ref"),
                r("srv")
            ),
        );
        let (e, dirs) = setup(&env);
        let mut notices = Vec::new();
        layout(&e, &dirs, &env.work(), false, &mut notices);
        assert!(
            notices
                .iter()
                .any(|(_, t)| t.contains("(never write)") && t.contains("srv/ref")),
            "{notices:?}"
        );
        assert!(
            !notices.iter().any(|(_, t)| t.contains("read and write")),
            "{notices:?}"
        );
    }

    #[test]
    fn extra_read_paths_are_announced_and_skipped_ones_flagged() {
        let env = TestEnv::new();
        env.write("data/tools/x", "x");
        env.write(
            "home/.oxa/agent/settings.json",
            &format!(
                r#"{{"oxa":{{"sandbox":{{"readPaths":["{}","{}"]}}}}}}"#,
                env.root().join("data/tools").display(),
                env.root().join("nope").display()
            ),
        );
        let (e, dirs) = setup(&env);
        let mut notices = Vec::new();
        layout(&e, &dirs, &env.work(), false, &mut notices);
        assert!(
            notices.iter().any(|(l, t)| *l == Level::Info
                && t.contains("can also read (never write)")
                && t.contains("data/tools")),
            "{notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|(l, t)| *l == Level::Warning && t.contains("nope")),
            "{notices:?}"
        );
    }

    #[test]
    fn missing_or_denied_child_and_writable_overlap_are_reported_truthfully() {
        let env = TestEnv::new();
        env.write("data/ok/x", "x");
        env.write("data/w/x", "x");
        env.write("home/.ssh/id", "k");
        let r = |p: &str| env.root().join(p).display().to_string();
        env.write(
            "home/.oxa/agent/settings.json",
            &format!(
                r#"{{"oxa":{{"sandbox":{{"readPaths":["{}","{}","{}","{}"],"writePaths":["{}"]}}}}}}"#,
                r("data/ok"),
                r("data/ok/missing"),
                env.home().join(".ssh").display(),
                r("data/w"),
                r("data/w")
            ),
        );
        let (e, dirs) = setup(&env);
        let mut notices = Vec::new();
        layout(&e, &dirs, &env.work(), false, &mut notices);
        let text = |lvl: Level| {
            notices
                .iter()
                .filter(|(l, _)| *l == lvl)
                .map(|(_, t)| t.clone())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let (info, warn) = (text(Level::Info), text(Level::Warning));
        let ro = info.lines().find(|l| l.contains("never write")).unwrap_or("");
        assert!(
            ro.contains("data/ok") && !ro.contains("missing") && !ro.contains(".ssh"),
            "{info}"
        );
        assert!(
            warn.contains("data/ok/missing") && warn.contains(".ssh"),
            "{warn}"
        );
        assert!(!ro.contains("data/w"), "{info}");
        assert!(
            info.contains("read and write") && info.contains("data/w"),
            "{info}"
        );
    }

    #[test]
    fn read_path_above_the_workspace_is_not_called_readable() {
        let env = TestEnv::new();
        env.write("data/app/x", "x");
        let r = |p: &str| env.root().join(p).display().to_string();
        env.write(
            "home/.oxa/agent/settings.json",
            &format!(r#"{{"oxa":{{"sandbox":{{"readPaths":["{}"]}}}}}}"#, r("data")),
        );
        let (e, dirs) = setup(&env);
        let mut notices = Vec::new();
        layout(&e, &dirs, &env.root().join("data/app"), false, &mut notices);
        assert!(
            !notices.iter().any(|(_, t)| t.contains("never write")),
            "{notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|(_, t)| t.contains("contains the workspace") && t.contains("data")),
            "{notices:?}"
        );
    }

    #[test]
    fn symlinked_read_path_above_the_workspace_is_not_called_readable() {
        let env = TestEnv::new();
        env.write("data/app/x", "x");
        std::os::unix::fs::symlink(env.root().join("data"), env.root().join("link")).unwrap();
        env.write(
            "home/.oxa/agent/settings.json",
            &format!(
                r#"{{"oxa":{{"sandbox":{{"readPaths":["{}"]}}}}}}"#,
                env.root().join("link").display()
            ),
        );
        let (e, dirs) = setup(&env);
        let mut notices = Vec::new();
        layout(&e, &dirs, &env.root().join("data/app"), false, &mut notices);
        assert!(
            !notices.iter().any(|(_, t)| t.contains("never write")),
            "{notices:?}"
        );
        assert!(
            notices.iter().any(|(_, t)| t.contains("contains the workspace")),
            "{notices:?}"
        );
    }

    fn socket_at(path: &std::path::Path) -> std::os::unix::net::UnixListener {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::net::UnixListener::bind(path).expect("bind socket")
    }

    #[test]
    fn listed_socket_is_in_the_layout_and_notice() {
        let env = TestEnv::new();
        let sock = env.root().join("rt/docker.sock");
        let _l = socket_at(&sock);
        env.write(
            "home/.oxa/agent/settings.json",
            &format!(r#"{{"oxa":{{"sandbox":{{"sockets":["{}"]}}}}}}"#, sock.display()),
        );
        let (e, dirs) = setup(&env);
        let mut notices = Vec::new();
        let l = layout(&e, &dirs, &env.work(), false, &mut notices);
        assert_eq!(l.sockets, vec![sock.canonicalize().unwrap()]);
        assert!(
            notices
                .iter()
                .any(|(lv, t)| *lv == Level::Warning && t.contains("docker.sock")),
            "{notices:?}"
        );
    }

    #[test]
    fn project_cannot_list_sockets() {
        let env = TestEnv::new();
        let sock = env.root().join("rt/docker.sock");
        let _l = socket_at(&sock);
        env.write(
            "work/.oxa/settings.json",
            &format!(r#"{{"oxa":{{"sandbox":{{"sockets":["{}"]}}}}}}"#, sock.display()),
        );
        let (e, dirs) = setup(&env);
        let mut notices = Vec::new();
        let l = layout(&e, &dirs, &env.work(), false, &mut notices);
        assert!(l.sockets.is_empty());
        assert!(!notices.iter().any(|(_, t)| t.contains("sockets")), "{notices:?}");
    }

    #[test]
    fn a_listed_path_that_is_not_a_socket_is_ignored_with_a_warning() {
        let env = TestEnv::new();
        let file = env.write("rt/plain", "x");
        env.write(
            "home/.oxa/agent/settings.json",
            &format!(r#"{{"oxa":{{"sandbox":{{"sockets":["{}"]}}}}}}"#, file.display()),
        );
        let (e, dirs) = setup(&env);
        let mut notices = Vec::new();
        let l = layout(&e, &dirs, &env.work(), false, &mut notices);
        assert!(l.sockets.is_empty());
        assert!(notices
            .iter()
            .any(|(lv, t)| *lv == Level::Warning && t.contains("ignored")));
    }

    #[test]
    fn socket_notice_names_the_risk() {
        let n = socket_notice(&[std::path::PathBuf::from("/run/docker.sock")]);
        assert!(
            n.contains("/run/docker.sock") && n.contains("root on this machine"),
            "{n}"
        );
        let n = socket_notice(&[std::path::PathBuf::from("/run/app/x.sock")]);
        assert!(n.contains("/run/app/x.sock") && n.contains("bash can do"), "{n}");
    }

    fn worker_env(env: &TestEnv, extra: &[(&str, &str)]) -> (Env, Dirs) {
        let mut owned = env.envs("http://127.0.0.1:9");
        owned.push(("OXA_UNATTENDED".to_string(), "1".to_string()));
        owned.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let pairs: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let e = Env::from_pairs(&pairs);
        let dirs = dirs_from_env(&e, None).expect("dirs");
        (e, dirs)
    }

    #[test]
    fn runner_rejects_helpers_in_implicit_worker_write_mounts() {
        use std::os::unix::fs::PermissionsExt;

        let env = TestEnv::new();
        std::fs::create_dir_all(env.work().join(".git")).unwrap();
        let registry = env.home().join(".cargo/registry");
        let sccache = env.home().join(".cache/custom-sccache");
        for dir in [&registry, &sccache] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let (e, dirs) = worker_env(&env, &[("SCCACHE_DIR", sccache.to_str().unwrap())]);
        let user = Settings::load(&dirs, &env.work(), false);
        // The runner's policy is established before the worker creates the shared target.
        let writes = worker_token_write_paths(&user, &e, &dirs);
        let worker = layout(&e, &dirs, &env.work(), false, &mut Vec::new());
        let target = PathBuf::from(target_of(&worker).expect("shared target"));
        let owner_tools = env.home().join("owner-tools");
        std::fs::create_dir_all(&owner_tools).unwrap();
        for (dir, allowed) in [
            (registry, false),
            (sccache, false),
            (target, false),
            (owner_tools, true),
        ] {
            let path = dir.join("mint");
            std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            let argv = [path.to_str().unwrap().to_owned()];
            let helper = oxa_work::trusted_token_program(&argv, None, Some(&env.work()), &writes, None);
            assert_eq!(helper.is_some(), allowed, "{}", path.display());
        }
    }

    fn target_of(l: &Layout) -> Option<String> {
        l.env
            .iter()
            .find(|(k, _)| k == "CARGO_TARGET_DIR")
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn a_worker_gets_exactly_the_derived_cache_path_writable_and_as_cargo_target() {
        let env = TestEnv::new();
        std::fs::create_dir_all(env.work().join(".git")).unwrap();
        let (e, dirs) = worker_env(&env, &[]);
        let plain = layout(&setup(&env).0, &dirs, &env.work(), false, &mut Vec::new());
        let mut notices = Vec::new();
        let l = layout(&e, &dirs, &env.work(), false, &mut notices);
        let target = std::path::PathBuf::from(target_of(&l).expect("cargo target"));
        assert!(target.is_dir() && target.ends_with("cargo-target"), "{target:?}");
        assert!(
            l.write.iter().any(|w| target.starts_with(w) || w == &target),
            "{:?}",
            l.write
        );
        assert_eq!(l.write.len(), plain.write.len() + 1, "{:?}", l.write);
        assert!(
            !l.write.iter().any(|w| w == target.parent().unwrap()),
            "no parent grant"
        );
        // A non-worker session gets nothing.
        assert!(target_of(&plain).is_none());
    }

    #[test]
    fn a_project_file_cannot_move_or_turn_off_the_cache_and_an_owner_target_wins() {
        let env = TestEnv::new();
        std::fs::create_dir_all(env.work().join(".git")).unwrap();
        env.write(
            "work/.oxa/settings.json",
            r#"{"oxa":{"workers":{"sharedCache":false,"cacheDir":"/tmp/elsewhere"}}}"#,
        );
        let (e, dirs) = worker_env(&env, &[]);
        let l = layout(&e, &dirs, &env.work(), false, &mut Vec::new());
        let target = target_of(&l).expect("project file cannot turn it off");
        assert!(!target.starts_with("/tmp/elsewhere"), "{target}");
        // The owner's own CARGO_TARGET_DIR wins and adds no write path.
        let (e, dirs) = worker_env(&env, &[("CARGO_TARGET_DIR", "/srv/own-target")]);
        let l = layout(&e, &dirs, &env.work(), false, &mut Vec::new());
        assert_eq!(target_of(&l).as_deref(), Some("/srv/own-target"));
        // The user setting turns it off.
        env.write(
            "home/.oxa/agent/settings.json",
            r#"{"oxa":{"workers":{"sharedCache":false}}}"#,
        );
        let (e, dirs) = worker_env(&env, &[]);
        let l = layout(&e, &dirs, &env.work(), false, &mut Vec::new());
        assert!(target_of(&l).is_none());
    }
}
