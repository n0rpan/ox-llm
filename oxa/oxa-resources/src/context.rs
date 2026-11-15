//! B7: context files and system prompt files.
// ported from pi packages/coding-agent/src/core/resource-loader.ts (context file discovery)
use crate::{ContextFile, Diagnostic, DiscoverOpts};
use oxa_config::Dirs;
use std::path::{Path, PathBuf};

/// File names read as context, in pi's per-directory preference order: the first one that
/// exists in a directory is that directory's context file.
pub const CONTEXT_FILE_NAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];
/// When present in a directory it is read *instead of* that directory's `AGENTS.md` /
/// `CLAUDE.md` (pi's same-directory rule).
pub const CONTEXT_OVERRIDE_NAME: &str = "AGENTS.override.md";
pub const SYSTEM_FILE: &str = "SYSTEM.md";
pub const APPEND_SYSTEM_FILE: &str = "APPEND_SYSTEM.md";

/// Result of [`load_context`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContextLoad {
    pub files: Vec<ContextFile>,
    pub system_override: Option<String>,
    pub append_system: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Load context files and the system prompt files.
///
/// * `files`: the agent dir's context file first, then one per directory from the outermost
///   ancestor of `cwd` down to `cwd` (pi's order). A file reached twice (agent dir is an
///   ancestor) is listed once. Empty with `opts.no_context`.
/// * `system_override`: `opts.system_prompt`, else `<cwd>/.oxa/SYSTEM.md` (trusted only), else
///   `<agent dir>/SYSTEM.md`.
/// * `append_system`: `<agent dir>/APPEND_SYSTEM.md`, then `<cwd>/.oxa/APPEND_SYSTEM.md`
///   (trusted only), then `opts.append_system_prompt` in order.
/// * Unreadable files become warnings and are skipped. Content is not trimmed or altered (it
///   is part of the frozen prefix bytes, W3).
pub fn load_context(dirs: &Dirs, cwd: &Path, opts: &DiscoverOpts) -> ContextLoad {
    let cwd = absolute(cwd);
    let mut out = ContextLoad::default();
    let project_dir = dirs.project_dir(&cwd);

    // Context files: agent dir first, then the ancestors of cwd, outermost first.
    if !opts.no_context {
        let mut search = vec![dirs.agent_dir.clone()];
        search.extend(context_search_dirs_in(&cwd, &dirs.home));
        let mut seen: Vec<PathBuf> = Vec::new();
        for dir in &search {
            let Some(path) = context_file_in(dir) else {
                continue;
            };
            let path = absolute(&path);
            let key = identity(&path);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            if let Some(content) = read_text(&path, &mut out.diagnostics) {
                out.files.push(ContextFile { path, content });
            }
        }
    }

    // SYSTEM.md: --system-prompt wins; else project (trusted only) over user.
    out.system_override = match &opts.system_prompt {
        Some(text) => Some(text.clone()),
        None => {
            let mut candidates = Vec::new();
            if opts.trusted {
                candidates.push(project_dir.join(SYSTEM_FILE));
            }
            candidates.push(dirs.agent_dir.join(SYSTEM_FILE));
            let chosen = candidates.into_iter().find(|p| p.is_file());
            chosen.and_then(|p| read_text(&p, &mut out.diagnostics))
        }
    };

    // APPEND_SYSTEM.md: user, then project (trusted only), then the --append-system-prompt flags.
    let mut append_files = vec![dirs.agent_dir.join(APPEND_SYSTEM_FILE)];
    if opts.trusted {
        append_files.push(project_dir.join(APPEND_SYSTEM_FILE));
    }
    for p in append_files.iter().filter(|p| p.is_file()) {
        if let Some(text) = read_text(p, &mut out.diagnostics) {
            out.append_system.push(text);
        }
    }
    out.append_system
        .extend(opts.append_system_prompt.iter().cloned());

    out
}

/// The context file of one directory under the same-directory rule, if any.
pub fn context_file_in(dir: &Path) -> Option<PathBuf> {
    let override_file = dir.join(CONTEXT_OVERRIDE_NAME);
    if override_file.is_file() {
        return Some(override_file);
    }
    CONTEXT_FILE_NAMES
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
}

/// The directories searched for context files, outermost first, ending with `cwd`, for the
/// process's `HOME` (see [`context_search_dirs_in`]).
pub fn context_search_dirs(cwd: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    context_search_dirs_in(cwd, &home)
}

/// The directories searched for context files, outermost first, ending with `cwd`: pi's walk
/// to the filesystem root, but never above or outside `home` (CT-10,
/// `oxa_proto::guard::project_ancestors`).
pub fn context_search_dirs_in(cwd: &Path, home: &Path) -> Vec<PathBuf> {
    let cwd = absolute(cwd);
    let mut dirs = oxa_proto::guard::project_ancestors(&cwd, home);
    dirs.reverse();
    dirs
}

/// Make a path absolute against the process working directory when it is relative.
fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(p))
        .unwrap_or_else(|_| p.to_path_buf())
}

/// Key used to recognise the same file reached by two routes (symlinks resolved when possible).
fn identity(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Read a file as text, exactly as on disk; invalid UTF-8 is replaced lossily. A read failure
/// becomes a warning and yields `None` (the file is skipped).
fn read_text(path: &Path, diagnostics: &mut Vec<Diagnostic>) -> Option<String> {
    match std::fs::read(path) {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) => {
            diagnostics.push(Diagnostic::warning(
                Some(path),
                format!("could not read context file, skipped: {e}"),
            ));
            None
        }
    }
}
