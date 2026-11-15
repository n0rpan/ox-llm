//! B2 / B3 / B4 (wave H lane `harden`): what the sandbox shows of the user's machine.
//!
//! * **Toolchains (B2).** The user's `PATH` is kept inside the sandbox (absolute entries that
//!   exist as directories, in order; the base `/usr/local/bin:/usr/bin:/bin` is appended when
//!   missing). Toolchain directories under `$HOME` are bound read-only and package caches
//!   read-write ([`TOOLCHAIN_READ`], [`CACHE_WRITE`], each only when it exists; plus every
//!   `PATH` entry under `$HOME`, read-only). The settings `oxa.sandbox.readPaths` /
//!   `oxa.sandbox.writePaths` (user settings only) add more. The environment variables that
//!   tell toolchains where they live ([`TOOLCHAIN_ENV`], `MISE_*`) are passed through when set,
//!   never a variable whose name looks like a secret ([`is_secret_name`]).
//! * **Deny-list (B3).** [`sensitive_paths`] (keys, tokens, trust, the agent directory,
//!   password stores, browser profiles) are masked after every bind: an empty read-only tmpfs
//!   over a directory, `/dev/null` over a file. Whatever the cwd or the settings, they are
//!   neither readable nor writable inside. The file tools obey the same list
//!   (`oxa-perm`'s `ModePolicy::with_guard`).
//! * **Home as cwd (B3, O-199).** Started in `/` or a parent of `$HOME`, bash gets no writable
//!   directory (the cwd is bound read-only), whatever the settings. Started in `$HOME` itself,
//!   the same unless the home is a workspace ([`Layout::allow_home_write`]: the user confirmed
//!   the interactive start there, or the user settings say `oxa.sandbox.allowHomeWrite: true`):
//!   then the home root and every dot-entry stay read-only and the plain folders and files of
//!   the home are writable (`bwrap::home_workspace_entries`, never `~/bin`, a symlink or a
//!   dot-entry: `.bashrc`, `.profile`, `.config/*`, `.local/bin`, `.ssh`, `.oxa`, ...); the
//!   deny-list holds either way.
//! * **Protected project files (B4).** Inside the cwd, the files that later run outside the
//!   sandbox ([`protected_paths`]: `.oxa`, `.agents/skills`, `.git/hooks`, `.git/config`) are
//!   bound read-only when they exist. For git data (sbx-hooks, see `gitdirs.rs`) a missing
//!   `hooks`, `config` or (with `extensions.worktreeConfig`) `config.worktree` of a real `.git`
//!   directory is masked so the command cannot create it, an existing `config.worktree` is
//!   read-only, and a worktree's `.git` file is read-only.
//!
//! The argv is deterministic: binds sorted by path, masks sorted by path.
use crate::gitdirs::GitBinds;
pub use oxa_proto::guard::{
    home_protected_paths, is_home_itself, is_home_or_above, protected_paths, sensitive_paths,
};
use std::path::{Path, PathBuf};

/// Setting: extra read-only binds (user settings only; `~/` expands to `$HOME`).
pub const SETTING_READ_PATHS: &str = "oxa.sandbox.readPaths";
/// Setting: extra read-write binds (user settings only).
pub const SETTING_WRITE_PATHS: &str = "oxa.sandbox.writePaths";
/// Setting: make `$HOME` a workspace when oxa runs in it (user settings only; see the module
/// docs). Never for `/` or a parent of `$HOME`. The deny-list still holds.
pub const SETTING_ALLOW_HOME_WRITE: &str = "oxa.sandbox.allowHomeWrite";

/// Fixed base `PATH` (appended when the user's lacks it).
pub const BASE_PATH: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin"];

/// Toolchain directories and files under `$HOME`, bound read-only when they exist.
pub const TOOLCHAIN_READ: &[&str] = &[
    ".cargo/bin",
    ".cargo/config.toml",
    ".cargo/config",
    ".rustup",
    ".local/bin",
    ".local/share/mise",
    ".config/mise",
    ".tool-versions",
    ".asdf",
    ".nvm",
    ".local/share/fnm",
    ".fnm",
    ".volta",
    "go/bin",
    ".local/share/pipx",
    ".local/pipx",
    ".local/share/uv",
    ".pyenv",
    ".rbenv",
    ".bun",
    ".deno",
    ".gitconfig",
    ".config/git",
];

/// Package caches under `$HOME` that tools must write to work, bound read-write when they
/// exist. A writable cache is shared with the user's own builds (see the crate README).
pub const CACHE_WRITE: &[&str] = &[
    ".cargo/registry",
    ".cargo/git",
    ".cache/mise",
    // Only `tracked-configs`: `trusted-configs` (trust records) must stay read-only.
    ".local/state/mise/tracked-configs",
    ".cache/pip",
    ".cache/uv",
    ".cache/go-build",
    ".cache/yarn",
    ".cache/pnpm",
    ".cache/deno",
    ".cache/node-gyp",
    ".npm",
    ".bun/install/cache",
    "go/pkg/mod",
];

/// Environment variables passed through when set (toolchain locations, never secrets).
pub const TOOLCHAIN_ENV: &[&str] = &[
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "GOPATH",
    "GOROOT",
    "GOMODCACHE",
    "GOCACHE",
    "ASDF_DIR",
    "ASDF_DATA_DIR",
    "NVM_DIR",
    "FNM_DIR",
    "VOLTA_HOME",
    "BUN_INSTALL",
    "DENO_DIR",
    "DENO_INSTALL",
    "PIPX_HOME",
    "PIPX_BIN_DIR",
    "UV_CACHE_DIR",
    "SCCACHE_DIR",
    "UV_PYTHON_INSTALL_DIR",
    "UV_TOOL_DIR",
    "PYENV_ROOT",
    "RBENV_ROOT",
    "PNPM_HOME",
    "npm_config_cache",
    "NPM_CONFIG_CACHE",
    "JAVA_HOME",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
];

/// Prefixes of variables passed through when set (the same secret rule applies).
pub const TOOLCHAIN_ENV_PREFIXES: &[&str] = &["MISE_"];

/// A variable name that looks like a secret (token, key, password, credential, auth, cookie,
/// session): never passed into the sandbox, whatever list it is on.
pub fn is_secret_name(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "KEY",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
        "COOKIE",
        "SESSION",
    ]
    .iter()
    .any(|w| up.contains(w))
}

/// What the sandbox shows of the machine. `Layout::default()` = the minimal layout: base
/// `PATH`, nothing bound beyond the cwd, the deny-list of `$HOME` only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Layout {
    /// `PATH` inside (empty = [`BASE_PATH`]).
    pub path: Vec<String>,
    /// Read-only binds (absolute, existing; sorted when built).
    pub read: Vec<PathBuf>,
    /// Read-write binds (absolute, existing; sorted when built).
    pub write: Vec<PathBuf>,
    /// The agent directory (part of the deny-list).
    pub agent_dir: Option<PathBuf>,
    /// Passed-through environment, sorted by name.
    pub env: Vec<(String, String)>,
    /// A home workspace when the cwd is `$HOME` itself (O-199): [`SETTING_ALLOW_HOME_WRITE`],
    /// or the user confirmed the interactive start in `$HOME`. No effect elsewhere.
    pub allow_home_write: bool,
    /// sbx-git: the git data of the cwd outside it (a worktree's gitdir and common dir, shared
    /// alternates), resolved once by `Bwrap::with_git_dirs`. Empty until then.
    pub git: GitBinds,
    /// sbx-ipc: mask the host runtime sockets (`/run/user/<uid>`, the buses, Docker/Podman,
    /// systemd-resolved) in the argv. Set by the front end ([`Bwrap::detect`] and the CLI's
    /// layout); off in the pure-argv tests, whose `Layout::default()` is not a sandbox.
    pub mask_runtime_ipc: bool,
}

/// The inputs of [`Layout::build`].
#[derive(Debug, Clone, Default)]
pub struct LayoutInputs {
    pub home: PathBuf,
    pub agent_dir: Option<PathBuf>,
    /// The user's `PATH`.
    pub path_var: String,
    /// The user's environment (name, value); only [`TOOLCHAIN_ENV`] / prefixed names are used.
    pub env: Vec<(String, String)>,
    /// [`SETTING_READ_PATHS`] / [`SETTING_WRITE_PATHS`] (`~/` expanded by [`expand_home`]).
    pub read_extra: Vec<PathBuf>,
    pub write_extra: Vec<PathBuf>,
    pub allow_home_write: bool,
    /// The running oxa binary: exposed read-only (when under `$HOME`, which is hidden) and
    /// named by `OXA_RUNNING_BIN`, so an agent can start the TUI in tmux from inside the sandbox.
    pub exe: Option<PathBuf>,
    /// The binary already lies inside the writable workspace: name it, but do not bind it
    /// (a bind makes the file a mount point, so a rebuild could not replace it).
    pub exe_in_workspace: bool,
}

/// `~` / `~/x` -> `$HOME` / `$HOME/x`; other paths unchanged.
pub fn expand_home(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(raw)
    }
}

fn denied(p: &Path, deny: &[PathBuf]) -> bool {
    deny.iter().any(|d| p.starts_with(d))
}

impl Layout {
    /// Whether `build` keeps an extra path (the same checks it applies to every configured
    /// path: absolute, existing, not a credential path, not the home itself or `/`).
    pub fn keeps_extra(home: &Path, agent_dir: Option<&Path>, p: &Path) -> bool {
        let deny = sensitive_paths(home, agent_dir);
        p.is_absolute() && p.exists() && !denied(p, &deny) && p != home && p != Path::new("/")
    }

    /// Whether some writable mount lies strictly inside `p`.
    pub fn has_writable_inside(&self, p: &Path) -> bool {
        self.write.iter().any(|w| w != p && w.starts_with(p))
    }

    /// Build the layout from the user's environment and settings (see the module docs).
    pub fn build(i: &LayoutInputs) -> Layout {
        let deny = sensitive_paths(&i.home, i.agent_dir.as_deref());
        let mut path: Vec<String> = Vec::new();
        let mut read: Vec<PathBuf> = Vec::new();
        for entry in i.path_var.split(':') {
            let p = Path::new(entry);
            if !p.is_absolute() || !p.is_dir() || denied(p, &deny) || path.iter().any(|x| x == entry) {
                continue;
            }
            path.push(entry.to_string());
            if p.starts_with(&i.home) && p != i.home {
                read.push(p.to_path_buf());
            }
        }
        for base in BASE_PATH {
            if !path.iter().any(|x| x == base) {
                path.push((*base).to_string());
            }
        }
        read.extend(TOOLCHAIN_READ.iter().map(|r| i.home.join(r)));
        read.extend(i.read_extra.iter().cloned());
        if !i.exe_in_workspace {
            read.extend(i.exe.iter().cloned());
        }
        let mut write: Vec<PathBuf> = CACHE_WRITE.iter().map(|w| i.home.join(w)).collect();
        write.extend(i.write_extra.iter().cloned());
        let keep = |p: &PathBuf| Layout::keeps_extra(&i.home, i.agent_dir.as_deref(), p);
        read.retain(keep);
        write.retain(keep);
        read.sort();
        read.dedup();
        write.sort();
        write.dedup();
        // A path both read and written is written.
        read.retain(|r| !write.contains(r));
        // A bind inside a bind of the same mode adds nothing (mise's PATH entries under
        // `~/.local/share/mise`): keep the outer one.
        let mut all: Vec<(PathBuf, bool)> = read
            .iter()
            .map(|p| (p.clone(), false))
            .chain(write.iter().map(|p| (p.clone(), true)))
            .collect();
        all.sort();
        let mut kept: Vec<(PathBuf, bool)> = Vec::new();
        for (p, rw) in all {
            let parent_mode = kept.iter().rev().find(|(k, _)| p.starts_with(k)).map(|(_, m)| *m);
            if parent_mode != Some(rw) {
                kept.push((p, rw));
            }
        }
        let read: Vec<PathBuf> = kept
            .iter()
            .filter(|(_, rw)| !rw)
            .map(|(p, _)| p.clone())
            .collect();
        let write: Vec<PathBuf> = kept
            .iter()
            .filter(|(_, rw)| *rw)
            .map(|(p, _)| p.clone())
            .collect();
        let mut env: Vec<(String, String)> = i
            .env
            .iter()
            .filter(|(k, _)| {
                (TOOLCHAIN_ENV.contains(&k.as_str())
                    || TOOLCHAIN_ENV_PREFIXES.iter().any(|p| k.starts_with(p)))
                    && !is_secret_name(k)
            })
            .cloned()
            .collect();
        // Version-manager shims (mise) write cache/state under the read-only home and warn on
        // every call; point them at the sandbox /tmp unless the caller set them.
        // A caller SCCACHE_DIR only survives when it lies on a writable bind (or /tmp);
        // otherwise it sits on the read-only home and sccache would fail.
        env.retain(|(k, v)| {
            k != "SCCACHE_DIR" || {
                let p = Path::new(v);
                p.starts_with("/tmp") || write.iter().any(|w| p.starts_with(w))
            }
        });
        for (k, v) in [
            ("MISE_CACHE_DIR", "/tmp/mise-cache"),
            ("MISE_STATE_DIR", "/tmp/mise-state"),
            // #362: `gh run view` creates its cache under the read-only home. Throwaway /tmp,
            // so nothing persists in the real home for an unsandboxed `gh` to read back.
            ("GH_CACHE_DIR", "/tmp/gh-cache"),
            ("SCCACHE_DIR", "/tmp/sccache"),
        ] {
            if !env.iter().any(|(n, _)| n == k) {
                env.push((k.to_string(), v.to_string()));
            }
        }
        if let Some(exe) = i
            .exe
            .as_ref()
            .filter(|e| e.is_absolute() && e.is_file() && !denied(e, &deny))
        {
            if !env.iter().any(|(n, _)| n == "OXA_RUNNING_BIN") {
                env.push(("OXA_RUNNING_BIN".to_string(), exe.display().to_string()));
            }
        }
        env.sort();
        env.dedup_by(|a, b| a.0 == b.0);
        Layout {
            path,
            read,
            write,
            agent_dir: i.agent_dir.clone(),
            env,
            allow_home_write: i.allow_home_write,
            git: GitBinds::default(),
            mask_runtime_ipc: false,
        }
    }

    /// The `PATH` value inside.
    pub fn path_value(&self) -> String {
        if self.path.is_empty() {
            BASE_PATH.join(":")
        } else {
            self.path.join(":")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sccache_gets_a_writable_dir_unless_the_caller_set_one() {
        let l = Layout::build(&LayoutInputs {
            home: PathBuf::from("/nonexistent-home"),
            ..Default::default()
        });
        assert!(l.env.contains(&("SCCACHE_DIR".into(), "/tmp/sccache".into())));
        // The host sccache store holds unverified compiled artifacts: never bound writable,
        // so a caller value inside it falls back to /tmp too.
        let l = Layout::build(&LayoutInputs {
            home: PathBuf::from("/h"),
            env: vec![("SCCACHE_DIR".into(), "/h/.cache/sccache".into())],
            ..Default::default()
        });
        assert!(l.env.contains(&("SCCACHE_DIR".into(), "/tmp/sccache".into())));
        assert!(!l.write.iter().any(|w| w.ends_with(".cache/sccache")));
        // A value elsewhere on the read-only home is replaced by a writable one.
        let l = Layout::build(&LayoutInputs {
            home: PathBuf::from("/h"),
            env: vec![("SCCACHE_DIR".into(), "/h/.sccache-elsewhere".into())],
            ..Default::default()
        });
        assert!(l.env.contains(&("SCCACHE_DIR".into(), "/tmp/sccache".into())));
    }

    #[test]
    fn the_running_binary_is_bound_and_named() {
        let d = std::env::temp_dir().join(format!("oxa-exe-{}", std::process::id()));
        let bin = d.join("home/.oxa/bin/oxa");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "x").unwrap();
        let l = Layout::build(&LayoutInputs {
            home: d.join("home"),
            exe: Some(bin.clone()),
            ..Default::default()
        });
        let _ = std::fs::remove_dir_all(&d);
        assert!(l.read.contains(&bin), "{:?}", l.read);
        assert!(l
            .env
            .contains(&("OXA_RUNNING_BIN".into(), bin.display().to_string())));
    }

    #[test]
    fn a_binary_in_the_workspace_is_named_not_bound() {
        let d = std::env::temp_dir().join(format!("oxa-exew-{}", std::process::id()));
        let bin = d.join("home/ws/target/release/oxa");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "x").unwrap();
        let l = Layout::build(&LayoutInputs {
            home: d.join("home"),
            exe: Some(bin.clone()),
            exe_in_workspace: true,
            ..Default::default()
        });
        let _ = std::fs::remove_dir_all(&d);
        assert!(!l.read.contains(&bin), "{:?}", l.read);
        assert!(l.env.iter().any(|(k, _)| k == "OXA_RUNNING_BIN"));
        assert!(!l.env.iter().any(|(k, _)| k == "OXA_BIN"));
    }

    #[test]
    fn secret_names_are_recognised() {
        for k in [
            "GITHUB_TOKEN",
            "MISE_GITHUB_TOKEN",
            "BRAVE_SEARCH_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "NPM_AUTH",
            "MY_PASSWORD",
        ] {
            assert!(is_secret_name(k), "{k}");
        }
        for k in ["CARGO_HOME", "MISE_DATA_DIR", "GOPATH", "npm_config_cache"] {
            assert!(!is_secret_name(k), "{k}");
        }
    }

    #[test]
    fn home_or_above() {
        let home = Path::new("/home/u");
        assert!(is_home_or_above(Path::new("/home/u"), home));
        assert!(is_home_or_above(Path::new("/home"), home));
        assert!(is_home_or_above(Path::new("/"), home));
        assert!(!is_home_or_above(Path::new("/home/u/project"), home));
        assert!(!is_home_or_above(Path::new("/srv"), home));
    }

    #[test]
    fn expand() {
        let home = Path::new("/home/u");
        assert_eq!(expand_home("~/x", home), PathBuf::from("/home/u/x"));
        assert_eq!(expand_home("~", home), PathBuf::from("/home/u"));
        assert_eq!(expand_home("/opt/x", home), PathBuf::from("/opt/x"));
    }
}
