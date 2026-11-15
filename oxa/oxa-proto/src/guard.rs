//! B3 / B4 (wave H lane `harden`): the paths no tool may touch, shared by the bash sandbox
//! (`oxa-sandbox`: masks and read-only binds) and the file tools (`oxa-perm`: the policy).
//!
//! * [`sensitive_paths`]: keys, tokens, trust, the agent directory, password stores, browser
//!   profiles. Neither readable nor writable by any tool, whatever the cwd or the settings.
//! * [`protected_paths`]: project files that run outside the sandbox later (`.oxa`,
//!   `.agents/skills`, `.git/hooks`, `.git/config`). Read-only in the bash sandbox; the file
//!   tools ask before writing `.oxa` / `.agents/skills` and never write `.git/hooks` /
//!   `.git/config`.
use std::path::{Path, PathBuf};

/// Paths under `$HOME` that are never readable or writable inside the sandbox (B3).
pub const SENSITIVE_HOME: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".kube",
    ".docker/config.json",
    ".netrc",
    ".git-credentials",
    ".config/git/credentials",
    ".config/gh",
    ".config/hub",
    ".config/oxc",
    ".config/local-ai/llm/cloud-secrets.json",
    ".local/state/oxc",
    ".oxa/agent",
    ".pi/agent",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".vault-token",
    ".terraform.d/credentials.tfrc.json",
    ".password-store",
    ".local/share/keyrings",
    ".config/op",
    ".config/Bitwarden",
    ".pki",
    ".mozilla",
    ".thunderbird",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".config/microsoft-edge",
    ".config/vivaldi",
    ".config/opera",
    // Other agents' login tokens (oxa may run in the home directory, O-199 first run).
    ".codex/auth.json",
    ".claude/.credentials.json",
    ".dsh/.credentials.yaml",
    ".gemini/oauth_creds.json",
];

/// Under `$HOME`: what runs code or holds agent and login configuration later, outside the
/// sandbox (shell startup files, autostart and service units, the user's own `bin`
/// directories, the desktop's config, the agents' settings). Never writable by bash in a home
/// workspace (where the home root and every dot-entry stay read-only anyway, and only these
/// non-dot entries need naming), and the file tools ask before writing them when the cwd is
/// the home directory or above it (outside the cwd they ask anyway).
/// [`SENSITIVE_HOME`] holds on top of this.
pub const PROTECTED_IN_HOME: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_logout",
    ".profile",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".zlogout",
    ".xprofile",
    ".xinitrc",
    ".xsession",
    ".pam_environment",
    ".ssh",
    ".gnupg",
    ".gitconfig",
    ".config/systemd",
    ".config/autostart",
    ".config/environment.d",
    ".config/hypr",
    ".config/omarchy",
    ".config/fish",
    ".config/git",
    ".config/oxa",
    ".local/bin",
    ".local/share/applications",
    ".local/share/systemd",
    ".oxa",
    ".dsh",
    ".codex",
    ".claude",
    ".claude.json",
    "bin",
];

/// Inside the cwd: project files that run outside the sandbox later (B4), bound read-only.
pub const PROTECTED_IN_CWD: &[&str] = &[
    ".oxa",
    ".agents/skills",
    ".git/hooks",
    ".git/config",
    ".git/config.worktree",
];

/// Of [`PROTECTED_IN_CWD`]: the ones the file tools never write (git runs them unsandboxed on
/// the user's next git command); the others ask.
pub const GIT_PROTECTED_IN_CWD: &[&str] = &[".git/hooks", ".git/config", ".git/config.worktree"];

/// The deny-list for `home` and the agent directory `agent_dir` (which may live outside
/// `$HOME`), every entry whether or not it exists.
pub fn sensitive_paths(home: &Path, agent_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = SENSITIVE_HOME.iter().map(|p| home.join(p)).collect();
    if let Some(a) = agent_dir {
        out.push(a.to_path_buf());
    }
    out.sort();
    out.dedup();
    out
}

/// The paths of [`PROTECTED_IN_HOME`] under `home`, whether or not they exist.
pub fn home_protected_paths(home: &Path) -> Vec<PathBuf> {
    PROTECTED_IN_HOME.iter().map(|p| home.join(p)).collect()
}

/// The protected project files of `cwd` (B4), whether or not they exist.
pub fn protected_paths(cwd: &Path) -> Vec<PathBuf> {
    PROTECTED_IN_CWD.iter().map(|p| cwd.join(p)).collect()
}

/// `true` when `cwd` is `$HOME`, `/`, or an ancestor of `$HOME`.
pub fn is_home_or_above(cwd: &Path, home: &Path) -> bool {
    cwd == Path::new("/") || home.starts_with(cwd)
}

/// `true` when `cwd` is the home directory itself (not a parent of it, not `/`), also through
/// a symlink: the one home root that may become a workspace (O-199 first run).
pub fn is_home_itself(cwd: &Path, home: &Path) -> bool {
    if home == Path::new("/") {
        return false;
    }
    if cwd == home {
        return true;
    }
    match (std::fs::canonicalize(cwd), std::fs::canonicalize(home)) {
        (Ok(c), Ok(h)) => c == h,
        _ => false,
    }
}

/// CT-10 (lane `harden`): the directories a project resource walk visits from `cwd` upward,
/// nearest first. pi walks to the filesystem root (context files) or the git root (skills).
/// oxa keeps that walk but never leaves the user's home from inside it: when `cwd` is below
/// `home` (the HOME the process was given), `home` is the last directory visited, so a HOME
/// that sits inside another home never sees that home's files. Outside `home` the walk goes
/// on as pi's but stops before another user's home (`/etc/passwd`). User-level locations
/// (`~/.agents/skills`, the agent directory) come from `Dirs`, never from this walk.
pub fn project_ancestors(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    let under_home = home != Path::new("/") && cwd.starts_with(home);
    let others = if under_home { Vec::new() } else { other_homes(home) };
    let mut out = Vec::new();
    for dir in cwd.ancestors() {
        if others.iter().any(|o| o == dir) {
            break;
        }
        out.push(dir.to_path_buf());
        if under_home && dir == home {
            break;
        }
    }
    out
}

/// The home directories of `/etc/passwd` other than `home` and `/`.
fn other_homes(home: &Path) -> Vec<PathBuf> {
    let text = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    text.lines()
        .filter_map(|l| l.split(':').nth(5))
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p != Path::new("/") && p != home)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_walk_stops_at_home_and_never_goes_above_it() {
        let home = Path::new("/x/realhome/Work/scratch/home");
        let walk = project_ancestors(&home.join("proj/sub"), home);
        assert_eq!(
            walk,
            vec![home.join("proj/sub"), home.join("proj"), home.to_path_buf()]
        );
        // Outside the home: pi's walk to the root.
        let walk = project_ancestors(Path::new("/x/other/p"), home);
        assert_eq!(
            walk,
            vec![
                PathBuf::from("/x/other/p"),
                PathBuf::from("/x/other"),
                PathBuf::from("/x"),
                PathBuf::from("/")
            ]
        );
    }
}
