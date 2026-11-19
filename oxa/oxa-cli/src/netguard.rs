//! Wave H: the network decisions of the front end (O-167(b), P3, P4). Every
//! decision is here; `dispatch.rs` only calls in.
use oxa_config::{dirs_from_env, Env, Settings};
use oxa_proto::{Level, OxaError, Sandbox};
use oxa_sandbox::{sandbox_net_setting, NetSandbox, SETTING_SANDBOX_NET};
use std::path::Path;
use std::sync::Arc;

/// O-167(b): the process sandbox with the bash network decision (`oxa.sandbox.net`, default
/// on; always off while offline). Read once at startup.
pub(crate) fn with_net(sandbox: Arc<dyn Sandbox>, settings: &Settings) -> Arc<NetSandbox> {
    with_policy(sandbox, settings, true)
}

/// Acceptance commands keep the user's network policy without GitHub authentication.
pub(crate) fn without_github(sandbox: Arc<dyn Sandbox>, settings: &Settings) -> Arc<dyn Sandbox> {
    with_policy(sandbox, settings, false)
}

fn with_policy(sandbox: Arc<dyn Sandbox>, settings: &Settings, github_auth: bool) -> Arc<NetSandbox> {
    let runner_owns_github = std::env::var_os(oxa_work::RUNNER_GITHUB_ENV).is_some_and(|value| value == "1");
    let on = !runner_owns_github && sandbox_net_setting(settings.string(SETTING_SANDBOX_NET).as_deref());
    let offline = oxa_proto::offline();
    // The `gh` lookup can stall (a mise shim may try to install gh): run it lazily, at the first
    // bash command that has the network, never at startup.
    let lookup = github_auth
        && on
        && !offline
        && !test_isolated(
            std::env::var(oxa_proto::names::ENV_TEST_ISOLATION)
                .ok()
                .as_deref(),
        );
    let ns = NetSandbox::new(sandbox, on, offline);
    Arc::new(if lookup {
        ns.with_gh_token_lazy(gh_token)
    } else {
        ns
    })
}

/// Under test isolation (`OXA_TEST_ISOLATION=1`) the host's `gh` login is never consulted: a
/// test must not read real credentials, and a slow `gh` (up to `GH_DEADLINE`) must not delay
/// startup and skew timing tests.
fn test_isolated(v: Option<&str>) -> bool {
    v == Some("1")
}

/// #52: the GitHub CLI token for the networked sandbox, from `gh auth token` run once here,
/// outside the sandbox, with a short timeout. Any failure (no `gh`, not logged in, slow) is
/// `None`. The token is never logged.
fn gh_token() -> Option<String> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let exe = trusted_gh_for(
        std::env::var_os("PATH").as_deref(),
        std::env::current_dir().ok().as_deref(),
        home.as_deref(),
    )?;
    gh_token_from(&exe, GH_DEADLINE)
}

/// `trusted_gh` for a cwd that is the home directory (#122). The project rule cannot tell
/// `~/.local/bin` from `~/project/bin` there, so under `~` only the user's conventional
/// install directories (`USER_BIN`, including the mise shims) count; a `PATH` entry anywhere else below `~` (a writable
/// project shim) is refused, as are those whose resolved target is. System paths are unchanged,
/// and the writable-directory check of `trusted_gh` still applies.
fn trusted_gh_for(
    path: Option<&std::ffi::OsStr>,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Option<std::path::PathBuf> {
    trusted_executable_for("gh", path, cwd, home)
}

/// Host tools use the same project/PATH checks as the GitHub credential helper.
pub(crate) fn trusted_executable_for(
    program: &str,
    path: Option<&std::ffi::OsStr>,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Option<std::path::PathBuf> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let Some(h) = home.map(canon) else {
        return trusted_executable(program, path, cwd);
    };
    if !cwd.is_some_and(|c| canon(c) == h) {
        return trusted_executable(program, path, cwd);
    }
    let allowed: Vec<std::path::PathBuf> = USER_BIN.iter().map(|d| h.join(d)).collect();
    // Allowed by where it resolves to, so a symlink from `~/.local/bin` into a project is refused.
    let ok = |d: &Path| {
        let dc = canon(d);
        let under_home = |x: &Path| x.starts_with(&h) || x.starts_with(home.unwrap_or(&h));
        !(under_home(d) || under_home(&dc)) || allowed.iter().any(|a| dc.starts_with(a))
    };
    // The final executable must resolve to an allowed place too; otherwise keep searching.
    let file_ok = |f: &Path| {
        let fc = canon(f);
        let under_home = |x: &Path| x.starts_with(&h) || x.starts_with(home.unwrap_or(&h));
        !(under_home(f) || under_home(&fc)) || allowed.iter().any(|a| fc.starts_with(a))
    };
    std::env::split_paths(path?)
        .filter(|d| ok(d))
        .filter_map(|d| trusted_executable(program, Some(d.as_os_str()), None))
        .find(|f| file_ok(f))
}

/// The directories below `~` where a user installs their own tools.
const USER_BIN: &[&str] = &[
    ".local/bin",
    "bin",
    ".cargo/bin",
    ".local/share/mise/shims",
    ".local/share/mise/installs",
];

const GH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// Run `exe auth token --hostname github.com` under one `deadline` that covers the process and
/// the reading of its output.
fn gh_token_from(exe: &Path, deadline: std::time::Duration) -> Option<String> {
    let mut command = std::process::Command::new(exe);
    command.args(["auth", "token", "--hostname", "github.com"]);
    let (code, out) = oxa_work::command_runner(command, Some(4096), deadline)?;
    if code != 0 {
        return None;
    }
    let t = out.trim();
    (!t.is_empty() && !t.chars().any(char::is_whitespace)).then(|| t.to_string())
}

/// The host tool to run: the first executable with the requested name in an absolute `PATH` entry that is
/// not inside the project (`cwd`) and not a group- or world-writable directory. Relative
/// entries (`.`, `bin`, empty) are ignored, so a project shim never runs outside the sandbox.
fn trusted_executable(
    program: &str,
    path: Option<&std::ffi::OsStr>,
    cwd: Option<&Path>,
) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let cwd_c = cwd.and_then(|c| std::fs::canonicalize(c).ok());
    std::env::split_paths(path?)
        .filter(|d| d.is_absolute())
        .filter(|d| {
            let dc = std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
            !cwd.is_some_and(|c| d.starts_with(c) || dc.starts_with(c))
                && !cwd_c.as_ref().is_some_and(|c| dc.starts_with(c))
                && std::fs::metadata(&dc).is_ok_and(|m| m.permissions().mode() & 0o022 == 0)
        })
        .map(|d| d.join(program))
        .find(|p| {
            // The file's own resolved target must not lie in the project either.
            let pc = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            !cwd.is_some_and(|c| pc.starts_with(c))
                && !cwd_c.as_ref().is_some_and(|c| pc.starts_with(c))
                && std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Keep repository code out of host lookups in workers and check supervisors,
/// while preserving the order of the user's installed tool directories.
pub(crate) fn trusted_work_path(
    path: &std::ffi::OsStr,
    project: &Path,
    home: &Path,
) -> Option<std::ffi::OsString> {
    use std::os::unix::fs::PermissionsExt;
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    let project = project.canonicalize().ok()?;
    let is_worker = |p: &Path| p.components().any(|part| part.as_os_str() == ".oxa-worktrees");
    let allowed: Vec<_> = USER_BIN.iter().map(|d| home.join(d)).collect();
    let entries = std::env::split_paths(path).filter(|dir| {
        if !dir.is_absolute() || is_worker(dir) {
            return false;
        }
        let Ok(real) = dir.canonicalize() else {
            return false;
        };
        if is_worker(&real)
            || !std::fs::metadata(&real).is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o022 == 0)
        {
            return false;
        }
        if project == home {
            if real.starts_with(&home) && !allowed.iter().any(|p| real.starts_with(p)) {
                return false;
            }
        } else if dir.starts_with(&project) || real.starts_with(&project) {
            return false;
        }
        ["git", "gh", "bwrap"].iter().all(|name| {
            let exe = dir.join(name);
            if std::fs::symlink_metadata(&exe).is_err() {
                return true;
            }
            trusted_executable_for(name, Some(dir.as_os_str()), Some(&project), Some(&home))
                .is_some_and(|p| p.canonicalize().is_ok_and(|p| !is_worker(&p)))
        })
    });
    std::env::join_paths(entries).ok()
}

/// The setting that allows an engine host that is not loopback (P3). Read from the user's
/// settings only: a project file cannot send the prompts elsewhere.
pub(crate) const SETTING_ALLOW_REMOTE: &str = "oxa.engine.allowRemote";
/// The setting of the hard offline switch (P4).
pub(crate) const SETTING_OFFLINE: &str = "oxa.offline";
/// The environment variable of the hard offline switch (pi: `PI_OFFLINE`).
pub(crate) const ENV_OFFLINE: &str = "OXA_OFFLINE";

/// The shared user-facing boolean (`1`/`true`/`yes`/`on`), default `false`: unset, empty and
/// any other word stay off. See [`oxa_config::EnvFlag`].
pub(crate) fn truthy(v: Option<&str>) -> bool {
    oxa_config::EnvFlag::parse(v).or(false)
}

/// `(offline, allow_remote_engine)` from the flag, the environment and the user's settings.
/// Offline is on when any of `--offline`, `OXA_OFFLINE`, `oxa.offline` says so.
pub(crate) fn policy_from(flag_offline: bool, env: &Env, user: &Settings) -> (bool, bool) {
    let offline = flag_offline || truthy(env.get(ENV_OFFLINE)) || user.bool(SETTING_OFFLINE, false);
    (offline, user.bool(SETTING_ALLOW_REMOTE, false))
}

/// Install the process network policy (`oxa_proto::set_net_policy`) before anything connects.
pub(crate) fn install(flag_offline: bool, env: &Env, session_dir: Option<&Path>, cwd: &Path) {
    let user = match dirs_from_env(env, session_dir) {
        Ok(dirs) => Settings::load(&dirs, cwd, false),
        Err(_) => Settings::default(),
    };
    let (offline, allow_remote) = policy_from(flag_offline, env, &user);
    oxa_proto::set_net_policy(offline, allow_remote);
}

/// P3 at startup: the engine endpoint must be loopback unless `oxa.engine.allowRemote` (and
/// never a remote one while offline). The same check runs at every engine connect.
pub(crate) fn check_endpoint(base_url: &str) -> Result<(), OxaError> {
    match oxa_proto::url_host(base_url) {
        Some(host) => oxa_proto::check_engine_host(&host),
        None => Ok(()), // not a URL: `HttpEngine::new` reports it
    }
}

/// P3: the one startup notice for an allowed remote engine.
pub(crate) fn endpoint_notice(base_url: &str) -> Option<(Level, String)> {
    let host = oxa_proto::url_host(base_url)?;
    (!oxa_proto::is_loopback_host(&host)).then(|| (Level::Warning, oxa_proto::remote_engine_notice(&host)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn test_isolation_skips_the_host_gh_login() {
        assert!(test_isolated(Some("1")));
        assert!(!test_isolated(None));
        assert!(!test_isolated(Some("0")));
    }

    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("oxa-ng-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_helper_that_leaves_the_pipe_open_cannot_stall_startup() {
        let d = tmp("pipe");
        // Exits 0 at once, but a background child keeps stdout open for 6 s.
        let gh = script(&d, "gh", "(sleep 6 >&1 &)\necho SYNTH_TOKEN; exit 0");
        let t = std::time::Instant::now();
        let r = gh_token_from(&gh, std::time::Duration::from_millis(1500));
        assert!(
            t.elapsed() < std::time::Duration::from_secs(3),
            "blocked {:?}",
            t.elapsed()
        );
        assert!(r.is_none() || r.as_deref() == Some("SYNTH_TOKEN"));
    }

    #[test]
    fn a_well_behaved_helper_gives_its_token() {
        let d = tmp("ok");
        let gh = script(&d, "gh", "echo SYNTH_TOKEN");
        // A parallel test's fork can hold the script's write fd (ETXTBSY): retry the spawn.
        let got = (0..5).find_map(|_| gh_token_from(&gh, GH_DEADLINE));
        assert_eq!(got.as_deref(), Some("SYNTH_TOKEN"));
    }

    #[test]
    fn an_oversized_helper_result_is_refused() {
        let d = tmp("oversized");
        let gh = script(&d, "gh", "printf '%04097d\\n' 1");
        assert!(gh_token_from(&gh, GH_DEADLINE).is_none());
    }

    /// #122: with cwd = home, a `gh` in a user-owned directory under home (`~/.local/bin`)
    /// is still found, so `GH_TOKEN` is injected the same as for a project cwd.
    #[test]
    fn cwd_home_still_finds_gh_under_home() {
        let d = tmp("homecwd");
        let home = d.join("home");
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        script(&bin, "gh", "echo GOOD");
        let p = std::env::join_paths([bin.to_str().unwrap()]).unwrap();
        assert_eq!(
            trusted_gh_for(Some(&p), Some(&home), Some(&home)),
            Some(bin.join("gh"))
        );
        // A project below home is still excluded.
        let proj = home.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        script(&proj, "gh", "echo EVIL");
        let q = std::env::join_paths([proj.to_str().unwrap()]).unwrap();
        assert_eq!(trusted_gh_for(Some(&q), Some(&proj), Some(&home)), None);
    }

    /// #122 review: with cwd = home a writable project shim on PATH is still refused.
    #[test]
    fn cwd_home_refuses_a_project_shim_under_home() {
        let d = tmp("homeshim");
        let home = d.join("home");
        let proj = home.join("project/bin");
        std::fs::create_dir_all(&proj).unwrap();
        script(&proj, "gh", "echo EVIL");
        let p = std::env::join_paths([proj.to_str().unwrap()]).unwrap();
        assert_eq!(trusted_gh_for(Some(&p), Some(&home), Some(&home)), None);
        // ... also behind a symlink from an allowed directory.
        let lb = home.join(".local");
        std::fs::create_dir_all(&lb).unwrap();
        std::os::unix::fs::symlink(&proj, lb.join("bin")).unwrap();
        let q = std::env::join_paths([lb.join("bin").to_str().unwrap()]).unwrap();
        assert_eq!(trusted_gh_for(Some(&q), Some(&home), Some(&home)), None);
    }

    /// P1: the final executable is checked, not just its directory.
    #[test]
    fn cwd_home_refuses_a_gh_symlink_into_a_project_file() {
        let d = tmp("ghlink");
        let home = d.join("home");
        let proj = home.join("project/bin");
        let lb = home.join(".local/bin");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&lb).unwrap();
        script(&proj, "gh", "echo EVIL");
        std::os::unix::fs::symlink(proj.join("gh"), lb.join("gh")).unwrap();
        let p = std::env::join_paths([lb.to_str().unwrap()]).unwrap();
        assert_eq!(trusted_gh_for(Some(&p), Some(&home), Some(&home)), None);
    }

    /// Control: a real gh, and a symlink to another file inside the allowed dir, still work.
    #[test]
    fn cwd_home_accepts_a_real_gh_and_an_in_dir_symlink() {
        let d = tmp("ghreal");
        let home = d.join("home");
        let lb = home.join(".local/bin");
        std::fs::create_dir_all(&lb).unwrap();
        script(&lb, "gh", "echo GOOD");
        let p = std::env::join_paths([lb.to_str().unwrap()]).unwrap();
        assert_eq!(
            trusted_gh_for(Some(&p), Some(&home), Some(&home)),
            Some(lb.join("gh"))
        );
        let lb2 = home.join(".cargo/bin");
        std::fs::create_dir_all(&lb2).unwrap();
        script(&lb2, "gh-real", "echo GOOD");
        std::os::unix::fs::symlink(lb2.join("gh-real"), lb2.join("gh")).unwrap();
        let q = std::env::join_paths([lb2.to_str().unwrap()]).unwrap();
        assert_eq!(
            trusted_gh_for(Some(&q), Some(&home), Some(&home)),
            Some(lb2.join("gh"))
        );
    }

    /// A `gh` symlink in a good directory that points into the project is refused too.
    #[test]
    fn a_gh_symlink_into_the_project_is_refused() {
        let d = tmp("projlink");
        let (proj, good) = (d.join("proj"), d.join("good"));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&good).unwrap();
        script(&proj, "gh", "echo EVIL");
        std::os::unix::fs::symlink(proj.join("gh"), good.join("gh")).unwrap();
        let p = std::env::join_paths([good.to_str().unwrap()]).unwrap();
        assert_eq!(trusted_executable("gh", Some(&p), Some(&proj)), None);
    }

    #[test]
    fn a_project_or_relative_path_entry_never_supplies_gh() {
        let d = tmp("shim");
        let proj = d.join("proj");
        let good = d.join("good");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&good).unwrap();
        script(&proj, "gh", "echo EVIL");
        script(&good, "gh", "echo GOOD");
        let sep = |v: &[&str]| std::env::join_paths(v).unwrap();
        // Relative entries are skipped.
        let rel = sep(&["bin", ".", "", good.to_str().unwrap()]);
        assert_eq!(
            trusted_executable("gh", Some(&rel), Some(&proj)),
            Some(good.join("gh"))
        );
        // An absolute entry inside the project directory is skipped too.
        let p = sep(&[proj.to_str().unwrap(), good.to_str().unwrap()]);
        assert_eq!(
            trusted_executable("gh", Some(&p), Some(&proj)),
            Some(good.join("gh"))
        );
        // Only untrusted entries: nothing.
        let only = sep(&[proj.to_str().unwrap(), "rel"]);
        assert_eq!(trusted_executable("gh", Some(&only), Some(&proj)), None);
    }
}
