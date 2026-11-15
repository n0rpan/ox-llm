//! C7: tool selection and the file-mutation queue.
// ported from pi packages/coding-agent/src/core/tools/index.ts (selection) and file-mutation-queue.ts
use oxa_proto::{Exposure, Source, Tool, ToolSpec};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The inputs of tool selection (cli.md "Tools").
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolSelection {
    /// `defaultTools` from settings after `+x` / `-x` patches; `None` = all built-ins (oxa [P]).
    pub default_tools: Option<Vec<String>>,
    /// `--tools <list>`: exactly these (names or globs).
    pub tools: Option<Vec<String>>,
    /// `--exclude-tools` / `-xt` (names or globs).
    pub exclude: Vec<String>,
    /// `--no-builtin-tools` / `-nbt`.
    pub no_builtin: bool,
    /// `--no-tools` / `-nt`.
    pub no_tools: bool,
    /// `Settings::default_tools_edits`: the `+x` / `-x` entries when `defaultTools` only edits
    /// the default selection. Applied to the default-selection members from hook sets
    /// (`Tool::default_selected`), which `default_tools` cannot name. `None` = no such edits.
    pub default_edits: Option<Vec<String>>,
}

/// The tools of one epoch.
#[derive(Clone, Default)]
pub struct Toolset {
    /// Declared to the model (`Exposure::Direct`), sorted by name.
    pub direct: Vec<Arc<dyn Tool>>,
    /// Callable but not declared (`Codemode`, `Deferred`, and MCP tools that `--tools` kept
    /// without naming them): reachable through codemode / `call_tool` only.
    pub indirect: Vec<Arc<dyn Tool>>,
}

impl Toolset {
    /// Look a tool up by name among the direct tools.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.direct.iter().find(|t| t.spec().name == name)
    }
    pub fn get_indirect(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.indirect.iter().find(|t| t.spec().name == name)
    }
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.direct.iter().map(|t| t.spec().clone()).collect()
    }
}

/// Resolve the selection matrix of cli.md: start from `tools` (if given) else `default_tools`
/// (else all built-ins) plus all non-built-in tools; `no_builtin` drops built-ins unless named
/// in `tools`; `no_tools` drops everything not named in `tools`; `exclude` always wins; globs
/// (`mcp__*`) match names. `Hidden` tools are dropped. Unknown names are returned as warnings.
/// Resolved once per prefix epoch (W3).
///
/// Details of the matrix:
/// * Built-ins (`Source::Builtin`) are on by default. So are the hook tools that are members of
///   the default selection (`Tool::default_selected`, the `subagent` tool where O-146 offers
///   it): selected like a built-in (an allowlist must name them, `+name` / `-name` edits of the
///   default selection and `default_edits` keep or drop them, `no_builtin` and `no_tools` drop
///   them unless named in `tools`).
/// * Built-in extensions (`builtin:*`, such as
///   `codemode` and `tool_search`) are off unless named in `default_tools` or `tools`.
/// * `tools` made only of `+name` / `-name` entries patches the default selection instead of
///   being an allowlist. A mixed list keeps its plain names and warns about the rest.
/// * With `tools` given and no entry starting with `mcp__`, MCP tools stay callable (reach set
///   by their exposure) but are never declared directly. With an `mcp__` entry, `tools` filters
///   MCP tools too.
/// * Declared tools (`Exposure::Direct`) and callable-only tools (`Codemode`, `Deferred`) are
///   returned separately, both sorted by name. The order is independent of the input order.
pub fn resolve_toolset(all: &[Arc<dyn Tool>], sel: &ToolSelection) -> (Toolset, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let known = |pat: &str| all.iter().any(|t| glob_match(pat, &t.spec().name));
    let is_patch = |e: &str| e.starts_with('+') || e.starts_with('-');

    // A `--tools` list of only `+name` / `-name` entries patches the default selection.
    let mut default_tools = sel.default_tools.clone();
    // Default-selection members from hook sets: kept by the `defaultTools` edits unless removed.
    let mut default_edits = sel.default_edits.clone();
    let kept_by_edits = |edits: &Option<Vec<String>>, name: &str| {
        edits.as_ref().is_some_and(|e| {
            !e.iter()
                .rev()
                .find(|x| &x[1..] == name)
                .is_some_and(|x| x.starts_with('-'))
        })
    };
    let mut tools = sel.tools.clone();
    if let Some(list) = sel.tools.as_ref() {
        let patches = list.iter().filter(|e| is_patch(e)).count();
        if patches > 0 && patches == list.len() {
            // The default selection being edited: the `defaultTools` list (or the built-ins)
            // plus the members from hook sets it keeps; the edits are folded into the names.
            let mut names: Vec<String> = default_tools.clone().unwrap_or_else(|| builtin_names(all));
            for t in all.iter().filter(|t| t.default_selected()) {
                let name = &t.spec().name;
                let member = default_tools.is_none()
                    || default_tools.iter().flatten().any(|p| glob_match(p, name))
                    || kept_by_edits(&default_edits, name);
                if member && !names.contains(name) {
                    names.push(name.clone());
                }
            }
            default_edits = None;
            for entry in list {
                let name = &entry[1..];
                if name.is_empty() || name.contains('*') {
                    warnings.push(format!("invalid tool patch: {entry}"));
                    continue;
                }
                if !known(name) {
                    warnings.push(format!("unknown tool: {name}"));
                    continue;
                }
                if entry.starts_with('+') {
                    if !names.iter().any(|n| n.as_str() == name) {
                        names.push(name.to_string());
                    }
                } else {
                    names.retain(|n| n.as_str() != name);
                }
            }
            default_tools = Some(names);
            tools = None;
        } else if patches > 0 {
            warnings.push(
                "--tools mixes +name/-name entries with plain names; the +/- entries are ignored".to_string(),
            );
            tools = Some(list.iter().filter(|e| !is_patch(e)).cloned().collect());
        }
    }

    for entry in tools.iter().flatten() {
        if !entry.contains('*') && !known(entry) {
            warnings.push(format!("unknown tool in --tools: {entry}"));
        }
    }
    for entry in &sel.exclude {
        if !entry.contains('*') && !known(entry) {
            warnings.push(format!("unknown tool in --exclude-tools: {entry}"));
        }
    }
    for entry in default_tools.iter().flatten() {
        if !entry.contains('*') && !known(entry) {
            warnings.push(format!("unknown tool in defaultTools: {entry}"));
        }
    }

    let mcp_entries = tools.iter().flatten().any(|e| e.starts_with("mcp__"));
    let named = |name: &str| tools.iter().flatten().any(|p| glob_match(p, name));
    let in_default = |name: &str| default_tools.iter().flatten().any(|p| glob_match(p, name));
    let allowlist = tools.is_some();

    // EXT-083 (pi's tool-override.ts): an extension tool with a built-in's name replaces the
    // built-in, and is selected as the built-in would be. Resolved here, once per epoch (W3).
    let overrides =
        |t: &Arc<dyn Tool>| matches!(&t.spec().source, Source::Extension(p) if !p.starts_with("builtin:"));
    let overridden: HashSet<String> = all
        .iter()
        .filter(|t| overrides(t))
        .map(|t| t.spec().name.clone())
        .filter(|n| {
            all.iter()
                .any(|b| b.spec().source == Source::Builtin && b.spec().name == *n)
        })
        .collect();
    let mut direct: Vec<Arc<dyn Tool>> = Vec::new();
    let mut indirect: Vec<Arc<dyn Tool>> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for t in all {
        let spec = t.spec();
        if spec.exposure == Exposure::Hidden || sel.exclude.iter().any(|p| glob_match(p, &spec.name)) {
            continue;
        }
        if spec.source == Source::Builtin && overridden.contains(&spec.name) {
            continue;
        }
        if !seen.insert(spec.name.clone()) {
            continue;
        }
        let is_named = named(&spec.name);
        // (selected, may be declared to the model)
        let (selected, declarable) = match &spec.source {
            Source::Extension(_) if overridden.contains(&spec.name) => {
                let base = if sel.no_tools || allowlist {
                    is_named
                } else {
                    default_tools.is_none() || in_default(&spec.name)
                };
                (base && (!sel.no_builtin || is_named), true)
            }
            Source::Builtin => {
                let base = if sel.no_tools || allowlist {
                    is_named
                } else {
                    default_tools.is_none() || in_default(&spec.name)
                };
                (base && (!sel.no_builtin || is_named), true)
            }
            // A member of the default selection from a hook set (O-146 `subagent`): as a
            // built-in, and kept by `defaultTools` edits that do not remove it.
            _ if t.default_selected() => {
                let base = if sel.no_tools || allowlist {
                    is_named
                } else {
                    default_tools.is_none()
                        || in_default(&spec.name)
                        || kept_by_edits(&default_edits, &spec.name)
                };
                (base && (!sel.no_builtin || is_named), true)
            }
            Source::Extension(path) if path.starts_with("builtin:") => {
                let base = if sel.no_tools || allowlist {
                    is_named
                } else {
                    in_default(&spec.name)
                };
                (base, true)
            }
            Source::Extension(_) => ((!(sel.no_tools || allowlist)) || is_named, true),
            Source::Mcp(_) => {
                if sel.no_tools || mcp_entries {
                    (is_named, is_named)
                } else {
                    // Kept by `--tools` unless named or matched; only a matched one is declared.
                    (true, !allowlist || is_named)
                }
            }
        };
        if !selected {
            continue;
        }
        match spec.exposure {
            Exposure::Direct if declarable => direct.push(Arc::clone(t)),
            Exposure::Direct | Exposure::Codemode | Exposure::Deferred => indirect.push(Arc::clone(t)),
            Exposure::Hidden => {}
        }
    }
    direct.sort_by(|a, b| a.spec().name.cmp(&b.spec().name));
    indirect.sort_by(|a, b| a.spec().name.cmp(&b.spec().name));
    (Toolset { direct, indirect }, warnings)
}

/// The tools that drive the others and stay declared under `codemode.mode: "only"`.
const CODEMODE_DRIVERS: &[&str] = &["codemode", "tool_search", "call_tool"];

/// H (SET-019): pi's `codemode.mode` `"only"` (settings.md, extensions/codemode/tool.ts
/// `hiddenDeclarations`): while `codemode` is declared, every other declared tool (built-in,
/// extension, MCP direct) is left out of the declarations and stays callable from scripts.
/// Decided with the toolset at epoch start (W3). Nothing changes when `codemode` is not
/// declared or the mode is `"on"`.
pub(crate) fn apply_codemode_mode(set: &mut Toolset, settings: &oxa_config::Settings) {
    if settings.string("codemode.mode").as_deref() != Some("only") || set.get("codemode").is_none() {
        return;
    }
    let (keep, hide): (Vec<_>, Vec<_>) = std::mem::take(&mut set.direct)
        .into_iter()
        .partition(|t| CODEMODE_DRIVERS.contains(&t.spec().name.as_str()));
    set.direct = keep;
    set.indirect.extend(hide);
    set.indirect.sort_by(|a, b| a.spec().name.cmp(&b.spec().name));
}

/// Names of the `Source::Builtin` tools.
fn builtin_names(all: &[Arc<dyn Tool>]) -> Vec<String> {
    all.iter()
        .filter(|t| t.spec().source == Source::Builtin)
        .map(|t| t.spec().name.clone())
        .collect()
}

/// Shell-style match where `*` matches any run of characters; every other byte is literal.
fn glob_match(pattern: &str, name: &str) -> bool {
    let (p, s) = (pattern.as_bytes(), name.as_bytes());
    let (mut pi, mut si) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if pi < p.len() && p[pi] == s[si] {
            pi += 1;
            si += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Serialises mutations per file path: two write-class tool calls on the same path never run
/// concurrently; calls on different paths may. A path is identified by its lock key
/// ([`MutationQueue::key`]), so aliases of one file (a symlink, `./a` and `a`, `d/../a`) share
/// one lock.
#[derive(Debug, Default)]
pub struct MutationQueue {
    busy: Mutex<Vec<PathBuf>>,
    cv: std::sync::Condvar,
}

impl MutationQueue {
    pub fn new() -> MutationQueue {
        MutationQueue::default()
    }

    /// The lock identity of `path`: absolute (a relative path joins the process cwd), symlinks
    /// resolved and `.` / `..` removed. For a path that does not exist (a file about to be
    /// created) the longest existing ancestor is resolved and the rest is appended lexically,
    /// so `link/new.txt` and `dir/./new.txt` give the same key.
    pub fn key(path: &Path) -> PathBuf {
        use std::path::Component;
        let abs = match std::env::current_dir() {
            Ok(cwd) if path.is_relative() => cwd.join(path),
            _ => path.to_path_buf(),
        };
        if let Ok(c) = std::fs::canonicalize(&abs) {
            return c;
        }
        let comps: Vec<Component> = abs.components().collect();
        for i in (1..comps.len()).rev() {
            let prefix: PathBuf = comps[..i].iter().collect();
            if let Ok(mut out) = std::fs::canonicalize(&prefix) {
                for c in &comps[i..] {
                    match c {
                        Component::ParentDir => {
                            out.pop();
                        }
                        Component::Normal(x) => out.push(x),
                        _ => {}
                    }
                }
                return out;
            }
        }
        // Nothing resolves (not even the root): lexical normalisation only.
        let mut out = PathBuf::new();
        for c in &comps {
            match c {
                Component::ParentDir => {
                    out.pop();
                }
                Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        out
    }

    /// The lock keys of `paths` ([`MutationQueue::key`]), sorted and without duplicates: the
    /// order in which [`MutationQueue::with_keys`] takes them.
    pub fn keys<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Vec<PathBuf> {
        let mut keys: Vec<PathBuf> = paths.into_iter().map(MutationQueue::key).collect();
        keys.sort();
        keys.dedup();
        keys
    }

    /// Run `f` while holding the lock for `path` (blocking until it is free).
    pub fn with<R>(&self, path: &Path, f: impl FnOnce() -> R) -> R {
        self.with_keys(&[MutationQueue::key(path)], f)
    }

    /// Run `f` while holding every lock of `keys` (from [`MutationQueue::keys`]: canonical,
    /// sorted, unique). They are taken one by one in that order, so two callers with overlapping
    /// sets cannot deadlock and one caller never waits for itself. All are released on return
    /// or panic.
    pub fn with_keys<R>(&self, keys: &[PathBuf], f: impl FnOnce() -> R) -> R {
        debug_assert!(
            keys.windows(2).all(|w| w[0] < w[1]),
            "lock keys must be sorted and unique"
        );
        let mut held = Vec::with_capacity(keys.len());
        for key in keys {
            held.push(self.acquire(key.clone()));
        }
        let out = f();
        // Release in reverse order of acquisition.
        while held.pop().is_some() {}
        out
    }

    fn acquire(&self, key: PathBuf) -> Release<'_> {
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        while busy.contains(&key) {
            busy = self.cv.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        busy.push(key.clone());
        // Released on drop, so a panicking `f` does not leave the path locked.
        Release { queue: self, key }
    }
}

struct Release<'a> {
    queue: &'a MutationQueue,
    key: PathBuf,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        let mut busy = self.queue.busy.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = busy.iter().position(|p| p == &self.key) {
            busy.remove(i);
        }
        self.queue.cv.notify_all();
    }
}
