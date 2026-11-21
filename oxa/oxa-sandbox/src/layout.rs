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
use crate::scratch::Scratch;
pub use oxa_proto::guard::{
    home_protected_paths, is_home_itself, is_home_or_above, protected_paths, sensitive_paths,
};
use std::path::{Path, PathBuf};

/// Setting: extra read-only binds (user settings only; `~/` expands to `$HOME`).
pub const SETTING_READ_PATHS: &str = "oxa.sandbox.readPaths";
/// Setting: extra read-write binds (user settings only).
pub const SETTING_WRITE_PATHS: &str = "oxa.sandbox.writePaths";
/// Setting: host runtime sockets (docker, podman) bound into bash (user settings only,
/// default none; a project file cannot list them).
pub const SETTING_SOCKETS: &str = "oxa.sandbox.sockets";
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
    ".cache/sccache",
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
    "CARGO_TARGET_DIR",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "GOPATH",
    "GOROOT",
    "GOMODCACHE",
    "GOCACHE",
    "SCCACHE_DIR",
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
    "UV_PYTHON_INSTALL_DIR",
    "UV_TOOL_DIR",
    "PYENV_ROOT",
    "RBENV_ROOT",
    "PNPM_HOME",
    "npm_config_cache",
    "NPM_CONFIG_CACHE",
    "JAVA_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
];

/// Prefixes of variables passed through when set (the same secret rule applies).
pub const TOOLCHAIN_ENV_PREFIXES: &[&str] = &["MISE_"];

/// Names whose exact values are masked in host shell output.
pub fn is_host_secret_name(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    HOST_SECRET_NAMES.iter().any(|word| up.contains(word))
}

const HOST_SECRET_NAMES: [&str; 4] = ["TOKEN", "SECRET", "KEY", "PASSWORD"];

/// A variable name that looks like a secret (token, key, password, credential, auth, cookie,
/// session): never passed into the sandbox, whatever list it is on.
pub fn is_secret_name(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    HOST_SECRET_NAMES
        .iter()
        .chain(["PASSWD", "CREDENTIAL", "AUTH", "COOKIE", "SESSION"].iter())
        .any(|w| up.contains(w))
}

/// The variables of `env` that may reach a command: toolchain locations ([`TOOLCHAIN_ENV`],
/// `MISE_*`), never a name that looks like a secret ([`is_secret_name`]). Everything else of the
/// process environment stays out (bwrap `--clearenv`, the same off Linux).
pub fn toolchain_env(env: &[(String, String)]) -> Vec<(String, String)> {
    env.iter()
        .filter(|(k, _)| {
            (TOOLCHAIN_ENV.contains(&k.as_str()) || TOOLCHAIN_ENV_PREFIXES.iter().any(|p| k.starts_with(p)))
                && !is_secret_name(k)
        })
        .cloned()
        .collect()
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
    /// The owner-listed host sockets ([`SETTING_SOCKETS`]; absolute, canonical, existing,
    /// sorted by the front end) that stay reachable although the runtime masks cover them.
    pub sockets: Vec<PathBuf>,
    /// #457: the session directory, mounted at `/tmp` (`scratch.rs`). `None` = a fresh tmpfs
    /// per command.
    pub scratch: Option<std::sync::Arc<Scratch>>,
    /// #457: where each session's directory is created ([`crate::scratch_root`]; set by the
    /// front end). `None` = no session directories.
    pub scratch_root: Option<PathBuf>,
    /// #457: the user's cache home ([`crate::user_cache_home`]), whose read-write bound caches
    /// are linked from `/tmp/.cache`. Empty = none.
    pub cache_home: PathBuf,
    /// #486: warn (never block) once the session directory holds more than this many bytes
    /// (`oxa.sandbox.scratchWarnSize`). `None` = no warning.
    pub scratch_warn: Option<u64>,
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
        let cache_home = i
            .home
            .canonicalize()
            .unwrap_or_else(|_| i.home.clone())
            .join(".cache");
        let mut write: Vec<PathBuf> = CACHE_WRITE
            .iter()
            .filter(|w| {
                **w != ".cache/sccache"
                    || i.home
                        .join(w)
                        .canonicalize()
                        .is_ok_and(|d| d.starts_with(&cache_home) && d != cache_home)
            })
            .map(|w| i.home.join(w))
            .collect();
        write.extend(i.write_extra.iter().cloned());
        // Only the established cache area gets implicit sccache binds. A name, an empty
        // directory, or a snapshot of cache contents cannot prove that an outside tree
        // will never contain checkouts and their executable git hooks (#530).
        if let Some((_, d)) = i.env.iter().find(|(k, _)| k == "SCCACHE_DIR") {
            let home = i.home.canonicalize().unwrap_or_else(|_| i.home.clone());
            if let Some(d) = Path::new(d)
                .canonicalize()
                .ok()
                .filter(|_| Path::new(d).is_absolute())
            {
                if d.is_dir() && d.starts_with(home.join(".cache")) && d != home.join(".cache") {
                    write.push(d);
                }
            }
        }
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
        let mut env = toolchain_env(&i.env);
        // Version-manager shims (mise) write state under the read-only home and warn on
        // every call; point it at the sandbox /tmp unless the caller set it.
        if !env.iter().any(|(n, _)| n == "MISE_STATE_DIR") {
            env.push(("MISE_STATE_DIR".to_string(), "/tmp/mise-state".to_string()));
        }
        // #457: one writable temp and cache home for every tool (gh, pip, sccache, mise, ...):
        // the session directory at `/tmp`. Never a host cache path, which is read-only here;
        // the user's own `XDG_CACHE_HOME` keeps its read-write bound caches through the links
        // `with_scratch` makes (`cache_home`).
        // Preserve a custom sccache location only if the final layout already grants it
        // write access (including an explicit user writePaths bind). Otherwise redirect
        // transparently to the writable session cache, even on a fresh home or with a
        // differently named wrapper. Never create host directories just to build a layout.
        let sccache_dir = env
            .iter()
            .find(|(n, _)| n == "SCCACHE_DIR")
            .map(|(_, d)| Path::new(d))
            .filter(|d| d.is_absolute())
            .and_then(|d| d.canonicalize().ok())
            .filter(|d| {
                d.is_dir()
                    && write
                        .iter()
                        .any(|w| w.canonicalize().is_ok_and(|w| d.starts_with(w)))
            })
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "/tmp/.cache/sccache".to_string());
        // #435: with the host network shared, every sandbox (and the user's own builds) would
        // meet the same sccache server on its default loopback port, and that server runs the
        // compile in its own mount namespace, where this checkout may not exist. A Unix socket
        // in the private `/tmp` gives each sandbox its own server.
        env.retain(|(n, _)| {
            !matches!(
                n.as_str(),
                "TMPDIR" | "XDG_CACHE_HOME" | "SCCACHE_DIR" | "SCCACHE_SERVER_UDS" | "SCCACHE_SERVER_PORT"
            )
        });
        env.push(("SCCACHE_SERVER_UDS".to_string(), "/tmp/.sccache.sock".to_string()));
        env.push(("TMPDIR".to_string(), "/tmp".to_string()));
        env.push(("XDG_CACHE_HOME".to_string(), "/tmp/.cache".to_string()));
        env.push(("SCCACHE_DIR".to_string(), sccache_dir));
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
            sockets: Vec::new(),
            scratch: None,
            scratch_root: None,
            cache_home: crate::scratch::user_cache_home(&i.home, &i.env),
            scratch_warn: None,
        }
    }

    /// #457: mount `scratch` at `/tmp` for every command, and link the read-write bound
    /// caches of the user's cache home ([`Layout::cache_home`]) from its `.cache` (see
    /// `scratch.rs`).
    pub fn with_scratch(mut self, scratch: Scratch) -> Layout {
        scratch.link_caches(&self.cache_home, &self.write);
        self.scratch = Some(std::sync::Arc::new(scratch));
        self
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
    fn cargo_target_is_forwarded_without_granting_a_mount() {
        let directory = oxa_fake::TestEnv::new();
        let target = directory.home().join(".cache/oxa-worker-target");
        std::fs::create_dir_all(&target).unwrap();
        let entry = ("CARGO_TARGET_DIR".to_owned(), target.display().to_string());
        let mut inputs = LayoutInputs {
            home: directory.home().clone(),
            env: vec![entry.clone()],
            ..Default::default()
        };
        let layout = Layout::build(&inputs);
        assert!(layout.env.contains(&entry));
        assert!(layout.write.iter().all(|path| !target.starts_with(path)));

        inputs.write_extra.push(target.clone());
        assert!(Layout::build(&inputs).write.contains(&target));
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
    fn sccache_dir_is_writable_when_it_exists() {
        let home = std::env::temp_dir().join(format!("oxa-sccache-{}", std::process::id()));
        let dir = home.join(".cache/sccache");
        std::fs::create_dir_all(&dir).unwrap();
        let l = Layout::build(&LayoutInputs {
            home: home.clone(),
            agent_dir: None,
            path_var: String::new(),
            env: vec![],
            read_extra: vec![],
            write_extra: vec![],
            allow_home_write: false,
            exe: None,
            exe_in_workspace: false,
        });
        let ok = l.write.contains(&dir);
        std::fs::remove_dir_all(&home).ok();
        assert!(ok, "~/.cache/sccache must be bound read-write");
    }

    #[test]
    fn custom_sccache_dir_is_bound_and_passed() {
        // A custom cache under the cache home stays shared after it is populated.
        let root = std::env::temp_dir().join(format!("oxa-sccache2-{}", std::process::id()));
        let home = root.join("home/u");
        let dir = home.join(".cache/build-cache");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let l = Layout::build(&LayoutInputs {
            home: home.clone(),
            agent_dir: None,
            path_var: String::new(),
            env: vec![("SCCACHE_DIR".into(), dir.display().to_string())],
            read_extra: vec![],
            write_extra: vec![],
            allow_home_write: false,
            exe: None,
            exe_in_workspace: false,
        });
        let bound = l.write.contains(&dir);
        let passed = l
            .env
            .iter()
            .any(|(k, v)| k == "SCCACHE_DIR" && *v == dir.display().to_string());
        std::fs::remove_dir_all(&root).ok();
        assert!(bound, "SCCACHE_DIR must be bound read-write");
        assert!(passed, "SCCACHE_DIR must be passed through");
    }

    #[test]
    fn unrelated_outside_sccache_dir_is_not_bound() {
        // #530: neither names nor emptiness establish that a tree is safe to bind.
        let root = std::env::temp_dir().join(format!("oxa-sccache5-{}", std::process::id()));
        let home = root.join("home/u");
        let work = root.join("data/work");
        std::fs::create_dir_all(work.join("checkout/.git/hooks")).unwrap();
        std::fs::create_dir_all(root.join("src/sccache/.git/hooks")).unwrap();
        std::fs::create_dir_all(root.join("data/sccache-work/checkout/.git/hooks")).unwrap();
        std::fs::create_dir_all(root.join("data/fresh")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let work = work.canonicalize().unwrap();
        let mut bad = vec![
            work.clone(),
            work.join("checkout"),
            root.join("src/sccache").canonicalize().unwrap(),
            root.join("data/sccache-work").canonicalize().unwrap(),
            root.join("data/fresh").canonicalize().unwrap(),
        ];
        bad.push(PathBuf::from("/var/tmp"));
        for b in bad {
            let l = Layout::build(&LayoutInputs {
                home: home.clone(),
                agent_dir: None,
                path_var: String::new(),
                env: vec![("SCCACHE_DIR".into(), b.display().to_string())],
                read_extra: vec![],
                write_extra: vec![],
                allow_home_write: false,
                exe: None,
                exe_in_workspace: false,
            });
            assert!(!l.write.contains(&b), "{} must not be writable", b.display());
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn outside_sccache_dir_uses_a_stable_fallback_unless_explicitly_bound() {
        let env = oxa_fake::TestEnv::new();
        let dir = env.root().join("data/build-cache");
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let mut inputs = LayoutInputs {
            home: env.home(),
            env: vec![("SCCACHE_DIR".into(), dir.display().to_string())],
            ..Default::default()
        };
        // Neither the implicit fallback nor the explicit sharing depends on cache contents.
        for populated in [false, true] {
            if populated {
                std::fs::create_dir_all(dir.join("a/b")).unwrap();
                std::fs::write(dir.join("a/b/object"), "cached object").unwrap();
            }
            inputs.write_extra.clear();
            let l = Layout::build(&inputs);
            assert!(!l.write.iter().any(|w| dir.starts_with(w)));
            assert!(l
                .env
                .contains(&("SCCACHE_DIR".into(), "/tmp/.cache/sccache".into())));

            inputs.write_extra.push(dir.clone());
            let l = Layout::build(&inputs);
            assert!(l.write.contains(&dir));
            assert!(l.env.contains(&("SCCACHE_DIR".into(), dir.display().to_string())));
        }
    }

    /// #435: sandboxes that share the host network (bash network on) also shared the sccache
    /// server's loopback port, so one sandbox's compile ran in another sandbox's server, whose
    /// mount namespace has no such checkout ("error writing dependencies ... No such file or
    /// directory"). The server socket lives in the private `/tmp`, whatever the caller set.
    #[test]
    fn sccache_server_socket_is_private_to_the_sandbox() {
        for user in [
            vec![],
            vec![(
                "SCCACHE_SERVER_UDS".to_string(),
                "/run/user/1000/shared.sock".to_string(),
            )],
            vec![("SCCACHE_SERVER_PORT".to_string(), "4226".to_string())],
        ] {
            let env = oxa_fake::TestEnv::new();
            let l = Layout::build(&LayoutInputs {
                home: env.home(),
                env: user,
                ..Default::default()
            });
            let uds: Vec<_> = l.env.iter().filter(|(k, _)| k == "SCCACHE_SERVER_UDS").collect();
            assert_eq!(
                uds,
                [&("SCCACHE_SERVER_UDS".to_string(), "/tmp/.sccache.sock".to_string())]
            );
            assert!(
                !l.env.iter().any(|(k, _)| k == "SCCACHE_SERVER_PORT"),
                "{:?}",
                l.env
            );
        }
    }

    #[test]
    fn fresh_sccache_uses_session_cache_without_host_mutation_for_any_wrapper() {
        for wrapper in [
            None,
            Some("/usr/bin/sccache"),
            Some("/usr/bin/sccache-dist"),
            Some("/usr/bin/rust-shim"),
        ] {
            let env = oxa_fake::TestEnv::new();
            let l = Layout::build(&LayoutInputs {
                home: env.home(),
                env: wrapper
                    .map(|w| vec![("RUSTC_WRAPPER".into(), w.into())])
                    .unwrap_or_default(),
                ..Default::default()
            });
            assert!(
                !env.home().join(".cache").exists(),
                "build must not mutate the host"
            );
            assert!(l
                .env
                .contains(&("SCCACHE_DIR".into(), "/tmp/.cache/sccache".into())));
        }
    }

    #[test]
    fn sccache_default_symlink_must_not_expose_an_outside_tree() {
        let env = oxa_fake::TestEnv::new();
        let outside = env.root().join("data/sccache-work");
        std::fs::create_dir_all(outside.join("checkout/.git/hooks")).unwrap();
        std::fs::create_dir_all(env.home().join(".cache")).unwrap();
        let alias = env.home().join(".cache/sccache");
        std::os::unix::fs::symlink(&outside, &alias).unwrap();
        for user_env in [vec![], vec![("SCCACHE_DIR".into(), alias.display().to_string())]] {
            let l = Layout::build(&LayoutInputs {
                home: env.home(),
                env: user_env,
                ..Default::default()
            });
            assert!(!l.write.iter().any(|w| w.canonicalize().unwrap() == outside));
            assert!(l
                .env
                .contains(&("SCCACHE_DIR".into(), "/tmp/.cache/sccache".into())));
        }
    }

    #[test]
    fn custom_home_sccache_stays_shared_after_population() {
        let env = oxa_fake::TestEnv::new();
        let dir = env.home().join(".cache/build-cache");
        std::fs::create_dir_all(&dir).unwrap();
        let inputs = LayoutInputs {
            home: env.home(),
            env: vec![("SCCACHE_DIR".into(), dir.display().to_string())],
            ..Default::default()
        };
        for populated in [false, true] {
            if populated {
                std::fs::create_dir_all(dir.join("a/b")).unwrap();
                std::fs::write(dir.join("a/b/object"), "cached object").unwrap();
            }
            let l = Layout::build(&inputs);
            assert!(l.write.contains(&dir));
            assert!(l.env.contains(&("SCCACHE_DIR".into(), dir.display().to_string())));
        }
    }

    #[test]
    fn wide_sccache_dir_is_not_bound() {
        let home = std::env::temp_dir().join(format!("oxa-sccache3-{}/home/u", std::process::id()));
        let root = home.parent().unwrap().parent().unwrap().to_path_buf();
        std::fs::create_dir_all(home.join(".config/sccache")).unwrap();
        std::fs::create_dir_all(home.join("elsewhere/my-sccache")).unwrap();
        for bad in [
            home.clone(),
            home.parent().unwrap().to_path_buf(),
            home.join(".config"),
            // sccache's own configuration lives here: never writable, whatever its name.
            home.join(".config/sccache"),
            home.join("elsewhere/my-sccache"),
            PathBuf::from("/etc"),
            PathBuf::from("/usr"),
            PathBuf::from("/"),
        ] {
            let l = Layout::build(&LayoutInputs {
                home: home.clone(),
                agent_dir: None,
                path_var: String::new(),
                env: vec![("SCCACHE_DIR".into(), bad.display().to_string())],
                read_extra: vec![],
                write_extra: vec![],
                allow_home_write: false,
                exe: None,
                exe_in_workspace: false,
            });
            assert!(!l.write.contains(&bad), "{} must not be writable", bad.display());
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sccache_dir_escaping_the_cache_is_not_bound() {
        let home = std::env::temp_dir().join(format!("oxa-sccache4-{}/home/u", std::process::id()));
        let root = home.parent().unwrap().parent().unwrap().to_path_buf();
        std::fs::create_dir_all(home.join(".cache")).unwrap();
        std::fs::create_dir_all(home.join(".config/systemd")).unwrap();
        // A symlink inside the cache that points at the home itself.
        std::os::unix::fs::symlink(&home, home.join(".cache/my-sccache")).unwrap();
        // A symlink inside the cache that points into the config tree.
        std::fs::create_dir_all(home.join(".config/sccache")).unwrap();
        std::os::unix::fs::symlink(home.join(".config/sccache"), home.join(".cache/sccache-link")).unwrap();
        let cases = [
            (home.join(".cache/sccache-link"), home.join(".config/sccache")),
            (
                home.join(".cache/../.config/systemd"),
                home.join(".config/systemd"),
            ),
            (home.join(".cache/my-sccache"), home.clone()),
        ];
        for (bad, real) in cases {
            let l = Layout::build(&LayoutInputs {
                home: home.clone(),
                agent_dir: None,
                path_var: String::new(),
                env: vec![("SCCACHE_DIR".into(), bad.display().to_string())],
                read_extra: vec![],
                write_extra: vec![],
                allow_home_write: false,
                exe: None,
                exe_in_workspace: false,
            });
            let hit = l
                .write
                .iter()
                .any(|w| *w == bad || w.canonicalize().ok() == real.canonicalize().ok());
            assert!(!hit, "{} must not be writable", bad.display());
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn expand() {
        let home = Path::new("/home/u");
        assert_eq!(expand_home("~/x", home), PathBuf::from("/home/u/x"));
        assert_eq!(expand_home("~", home), PathBuf::from("/home/u"));
        assert_eq!(expand_home("/opt/x", home), PathBuf::from("/opt/x"));
    }
}
