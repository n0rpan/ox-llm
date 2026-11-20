//! Owner decision (2026-11-19): a workspace or auto worker may publish its own branch without
//! asking, and nothing wider. This module is the "wider" half: it finds the simple commands of a
//! line that publish or mutate GitHub beyond that, and the floor prompt of `danger.rs` asks (or
//! routes to the lead's approvals desk) for them. Yolo is unchanged: it never reaches this rule.
//!
//! Allowed without a question (they produce no hit): `git push [origin [HEAD|<branch>|
//! HEAD:<branch>]]` of the session repo's current non-default branch with only `-u`/`-q`/`-v`
//! flags; `gh pr create|edit|comment` with no PR argument, repo override, head override or
//! body file; read-only `gh` (`pr view|checks|diff|status|list`, `run view|watch|list`,
//! `issue view|list`, `repo view`, `api` GET). A line that mixes such a command with any other
//! command outside a short local list (`cd`, `git status`, `head`...) is a hit on the other
//! command: the network permission of a publish command never covers a neighbour.
use crate::danger::Stage;
use std::path::{Path, PathBuf};

/// Names (the first word) that cannot reach the network or write outside the worktree on their
/// own, so they may sit next to a publish command.
const LOCAL: &[&str] = &[
    "cd", "ls", "cat", "echo", "printf", "head", "tail", "grep", "wc", "sort", "pwd", "true", "test", "[",
    "sleep", "date",
];
/// `git` subcommands that stay local.
const GIT_LOCAL: &[&str] = &[
    "status",
    "log",
    "diff",
    "add",
    "commit",
    "branch",
    "rev-parse",
    "show",
    "switch",
    "checkout",
    "stash",
    "restore",
    "rev-list",
    "remote",
];
const DEFAULT_BRANCHES: &[&str] = &["main", "master", "trunk"];

/// What one simple command is, for the publish rule.
#[derive(PartialEq, Eq)]
enum Kind {
    /// A publish command inside the allowed set.
    Allowed,
    /// A publish or GitHub-mutating command outside it.
    Unsafe,
    /// Anything else.
    Other,
}

/// The segments of `stages` that need an approval, re-joined by the caller's `segment`.
pub(crate) fn publish_hits(stages: &[Stage], cwd: &Path) -> Vec<String> {
    let unordered = stages.iter().any(|st| st.background || st.mark.is_some());
    let mutation = stages.iter().any(|st| mutates_repo_state(&st.words));
    let kinds: Vec<(usize, Kind)> = {
        let mut moved = false;
        let mut out = Vec::new();
        for (i, st) in stages.iter().enumerate() {
            if st.mark.is_some() || st.words.is_empty() {
                continue;
            }
            let mut kind = kind_of(&st.words, cwd, moved);
            if matches!(first_word(&st.words), Some("cd" | "pushd" | "popd")) || mutates_repo_state(&st.words)
            {
                moved = true;
            }
            if kind == Kind::Allowed
                && (unordered || mutation || st.redirects.iter().any(|(op, t)| !harmless_redirect(op, t)))
            {
                kind = Kind::Unsafe;
            }
            out.push((i, kind));
        }
        out
    };
    let any_allowed = kinds.iter().any(|(_, k)| *k == Kind::Allowed);
    let mut hits = Vec::new();
    for (i, kind) in &kinds {
        let st = &stages[*i];
        let hit = match kind {
            Kind::Unsafe => true,
            Kind::Other => any_allowed && !is_local(&st.words, &st.redirects),
            Kind::Allowed => false,
        };
        if hit {
            hits.push(st.words.join(" "));
        }
    }
    hits
}

/// A redirect that writes no file: to `/dev/null` or a descriptor copy (`2>&1`).
fn harmless_redirect(op: &str, target: &str) -> bool {
    target == "/dev/null" || (op == ">&" && (target == "-" || target.chars().all(|c| c.is_ascii_digit())))
}

/// A `git` command that can change the checked-out branch or the repository config, so the
/// on-disk state seen by the classifier is stale for any later command of the line.
fn mutates_repo_state(words: &[String]) -> bool {
    let args: Vec<String> = words.iter().skip_while(|w| is_assignment(w)).cloned().collect();
    if args.first().map(|w| base(w)) != Some("git") {
        return false;
    }
    match git_sub(&args[1..]) {
        Some(sub) => matches!(
            sub.as_str(),
            "switch"
                | "checkout"
                | "branch"
                | "remote"
                | "stash"
                | "restore"
                | "config"
                | "reset"
                | "worktree"
        ),
        None => false,
    }
}

fn first_word(words: &[String]) -> Option<&str> {
    words.iter().map(String::as_str).find(|w| !is_assignment(w))
}

fn is_assignment(w: &str) -> bool {
    w.split_once('=').is_some_and(|(n, _)| {
        !n.is_empty()
            && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !n.starts_with(|c: char| c.is_ascii_digit())
    })
}

fn dynamic(w: &str) -> bool {
    w.contains(['$', '`', '*', '?', '~', '{', '<', '>', '('])
}

fn base(w: &str) -> &str {
    w.rsplit('/').next().unwrap_or(w)
}

fn is_local(words: &[String], redirects: &[(String, String)]) -> bool {
    if redirects.iter().any(|(op, t)| !harmless_redirect(op, t)) {
        return false;
    }
    if words.iter().any(|w| dynamic(w)) {
        return false;
    }
    let args: Vec<&String> = words.iter().skip_while(|w| is_assignment(w)).collect();
    match args.first().map(|w| w.as_str()) {
        Some("git") => {
            let rest: Vec<String> = args[1..].iter().map(|s| s.to_string()).collect();
            !git_globals(&rest).2 && git_sub(&rest).is_some_and(|sub| GIT_LOCAL.contains(&sub.as_str()))
        }
        Some(n) => {
            LOCAL.contains(&n)
                && !(n == "sort"
                    && args.iter().skip(1).any(|a| {
                        a.as_str() == "-o"
                            || a.starts_with("-o")
                            || a.as_str() == "--output"
                            || a.starts_with("--output=")
                    }))
        }
        None => true,
    }
}

/// The env prefix names that can redirect git or gh (or the program lookup).
fn env_unsafe(words: &[String]) -> bool {
    words.iter().take_while(|w| is_assignment(w)).any(|w| {
        let n = w.split('=').next().unwrap_or("");
        (n.starts_with("GIT_") && n != "GIT_TERMINAL_PROMPT")
            || n.starts_with("GH_")
            || n.starts_with("GITHUB_")
            || n.starts_with("SSH_")
            || n.starts_with("HTTP")
            || n.starts_with("LD_")
            || matches!(
                n,
                "PATH" | "HOME" | "XDG_CONFIG_HOME" | "EDITOR" | "PAGER" | "BROWSER"
            )
    })
}

fn kind_of(words: &[String], cwd: &Path, moved: bool) -> Kind {
    let args: Vec<String> = words.iter().skip_while(|w| is_assignment(w)).cloned().collect();
    let Some(name) = args.first().cloned() else {
        return Kind::Other;
    };
    if matches!(base(&name), "git" | "gh") && !matches!(name.as_str(), "git" | "gh") {
        return Kind::Unsafe;
    }
    match name.as_str() {
        "git" => git_kind(words, &args, cwd, moved),
        "gh" => gh_kind(words, &args, cwd, moved),
        // A wrapper or interpreter that carries git push / gh in its words hides it.
        _ => {
            let hides = args.iter().skip(1).any(|w| {
                // Quotes and backslashes can split a word (`pu''sh`); compare the plain text.
                let plain = w.replace(['\'', '"', '\\'], "");
                w == "gh"
                    || w.contains("gh ")
                    || w.ends_with("/gh")
                    || (plain.contains("git") && plain.contains("push"))
                    || (plain.contains("git") && w.contains(['$', '`']))
            }) || args.iter().enumerate().any(|(i, w)| {
                base(w) == "git"
                    && args[i + 1..]
                        .iter()
                        .any(|a| matches!(a.as_str(), "push" | "send-pack"))
            }) || args.iter().skip(1).any(|w| base(w) == "gh");
            if hides {
                Kind::Unsafe
            } else {
                Kind::Other
            }
        }
    }
}

/// Global options that change nothing about repository, config or program lookup.
const GIT_PLAIN_GLOBALS: &[&str] = &["--no-pager", "-P", "--no-optional-locks", "--no-advice"];

/// Parse git's global options: (index of the subcommand, `-C` directory, whether an option
/// that can redirect git (`-c`, `--config-env`, `--git-dir`, `--exec-path`, anything unknown)
/// was seen).
fn git_globals(rest: &[String]) -> (usize, Vec<String>, bool) {
    let mut k = 0;
    let mut dirs = Vec::new();
    let mut opaque = false;
    while k < rest.len() {
        let a = rest[k].as_str();
        if a == "-C" {
            if let Some(dir) = rest.get(k + 1) {
                dirs.push(dir.clone());
            } else {
                opaque = true;
            }
            k += 2;
        } else if matches!(
            a,
            "-c" | "--config-env" | "--git-dir" | "--work-tree" | "--namespace"
        ) {
            opaque = true;
            k += 2;
        } else if a.starts_with('-') {
            if !GIT_PLAIN_GLOBALS.contains(&a) {
                opaque = true;
            }
            k += 1;
        } else {
            break;
        }
    }
    (k, dirs, opaque)
}

/// The subcommand of a git argv (after the program), skipping global options.
fn git_sub(rest: &[String]) -> Option<String> {
    let (k, _, _) = git_globals(rest);
    rest.get(k).cloned()
}

fn git_kind(all: &[String], args: &[String], cwd: &Path, moved: bool) -> Kind {
    let rest = &args[1..];
    let (k, dirs, opaque) = git_globals(rest);
    let sub = rest.get(k).map(String::as_str);
    if sub == Some("send-pack") {
        return Kind::Unsafe;
    }
    if sub != Some("push") {
        // An alias can expand to a push; builtin names cannot be shadowed by one.
        if let Some(name) = sub {
            if !GIT_LOCAL.contains(&name) && is_alias(&rest[..k], name, cwd, &dirs) {
                return Kind::Unsafe;
            }
        }
        return Kind::Other;
    }
    let push = &rest[k + 1..];
    if opaque || moved || env_unsafe(all) || all.iter().any(|w| dynamic(w)) {
        return Kind::Unsafe;
    }
    let wd = if dirs.is_empty() {
        cwd.to_path_buf()
    } else {
        let mut p = cwd.to_path_buf();
        for d in &dirs {
            p = crate::paths::normalize(Path::new(d), &p);
        }
        if !crate::paths::is_inside(&p, cwd) {
            return Kind::Unsafe;
        }
        // A nested repository (submodule, clone) is not the session repository.
        match (git_dirs(&p), git_dirs(cwd)) {
            (Some((_, a)), Some((_, b))) if same_dir(&a, &b) => {}
            _ => return Kind::Unsafe,
        }
        p
    };
    let Some(current) = current_branch(&wd) else {
        return Kind::Unsafe;
    };
    if !valid_branch(&current) || !known_non_default(&wd, &current) {
        return Kind::Unsafe;
    }
    let mut pos: Vec<&str> = Vec::new();
    for a in push {
        match a.as_str() {
            "-u" | "--set-upstream" | "-q" | "--quiet" | "-v" | "--verbose" => {}
            s if s.starts_with('-') => return Kind::Unsafe,
            s => pos.push(s),
        }
    }
    if config_risky(&wd, &current, pos.len() < 2) || !urls_unchanged(cwd, &wd) {
        return Kind::Unsafe;
    }
    let ok = match pos.as_slice() {
        [] => true,
        ["origin"] => true,
        ["origin", spec] => {
            if let Some((src, dst)) = spec.split_once(':') {
                let src = src.strip_prefix("refs/heads/").unwrap_or(src);
                let dst = dst.strip_prefix("refs/heads/").unwrap_or(dst);
                if src != current && src != "HEAD" {
                    return Kind::Unsafe;
                }
                dst == current
            } else {
                let spec = spec.strip_prefix("refs/heads/").unwrap_or(spec);
                spec == "HEAD" || spec == current
            }
        }
        _ => false,
    };
    if ok {
        Kind::Allowed
    } else {
        Kind::Unsafe
    }
}

/// Flags of `gh pr create|edit|comment` that are safe for the session's own PR, by arity. Any
/// other flag, and any positional (a PR number, url or branch), is not.
fn pr_flags_ok(rest: &[String]) -> bool {
    const VALUE_LONG: &[&str] = &[
        "title",
        "body",
        "base",
        "label",
        "assignee",
        "reviewer",
        "milestone",
        "project",
        "add-label",
        "remove-label",
        "add-assignee",
        "remove-assignee",
        "add-reviewer",
        "remove-reviewer",
        "add-project",
        "remove-project",
    ];
    const BOOL_LONG: &[&str] = &[
        "draft",
        "fill",
        "fill-first",
        "fill-verbose",
        "no-maintainer-edit",
        "web",
        "dry-run",
        "remove-milestone",
        "edit-last",
        "delete-last",
        "yes",
        "create-if-none",
        "editor",
    ];
    const VALUE_SHORT: &[char] = &['t', 'b', 'B', 'l', 'a', 'r', 'm', 'p'];
    const BOOL_SHORT: &[char] = &['d', 'f', 'w', 'e'];
    let mut j = 0;
    while j < rest.len() {
        let a = rest[j].as_str();
        if let Some(long) = a.strip_prefix("--") {
            match long.split_once('=') {
                Some((n, _)) if VALUE_LONG.contains(&n) => {}
                Some(_) => return false,
                None if VALUE_LONG.contains(&long) => j += 1,
                None if BOOL_LONG.contains(&long) => {}
                None => return false,
            }
        } else if let Some(short) = a.strip_prefix('-') {
            let mut cs = short.chars();
            match (cs.next(), cs.next()) {
                (Some(c), None) if VALUE_SHORT.contains(&c) => j += 1,
                (Some(c), None) if BOOL_SHORT.contains(&c) => {}
                (Some(c), Some(_)) if VALUE_SHORT.contains(&c) => {}
                _ => return false,
            }
        } else {
            return false;
        }
        j += 1;
    }
    true
}

fn valid_branch(b: &str) -> bool {
    !b.is_empty()
        && !b.starts_with(['-', '+', '/'])
        && !b.contains("..")
        && b.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

fn gh_kind(all: &[String], args: &[String], cwd: &Path, moved: bool) -> Kind {
    if env_unsafe(all) || all.iter().any(|w| dynamic(w)) {
        return Kind::Unsafe;
    }
    let rest = &args[1..];
    // Repo and head overrides, in any spelling (`-R x`, `-Rx`, `--repo=x`, `-Hx`, `--head x`).
    if rest.iter().any(|a| {
        a.starts_with("-R")
            || a.starts_with("-H")
            || a.starts_with("--repo")
            || a.starts_with("--head")
            || a.starts_with("--hostname")
    }) {
        return Kind::Unsafe;
    }
    let plain = |i: usize| {
        rest.get(i)
            .map(String::as_str)
            .filter(|a| !a.starts_with('-'))
            .unwrap_or("")
    };
    let (g, s) = (plain(0), plain(1));
    match (g, s) {
        ("pr", "create" | "edit" | "comment") => {
            // gh resolves the target repository from the git remotes: they must still be the
            // recorded ones.
            if moved || !pr_flags_ok(&rest[2..]) || !urls_unchanged(cwd, cwd) {
                Kind::Unsafe
            } else {
                Kind::Allowed
            }
        }
        ("pr", "view" | "checks" | "diff" | "status" | "list") => Kind::Allowed,
        ("run", "view" | "watch" | "list") => Kind::Allowed,
        ("issue", "view" | "list") | ("repo", "view") => Kind::Allowed,
        ("auth", "status") => Kind::Allowed,
        ("api", _) => {
            let mutating = rest.iter().any(|a| {
                // `-f`/`-F` in any short cluster or attached form (`-ftitle=x`), and the long
                // spellings with or without `=`.
                (a.starts_with('-') && !a.starts_with("--") && a.contains(['f', 'F']))
                    || ["--field", "--raw-field", "--input"]
                        .iter()
                        .any(|l| a.as_str() == *l || a.starts_with(&format!("{l}=")))
            }) || api_short_method_is_mutating(rest)
                || rest
                    .windows(2)
                    .any(|w| matches!(w[0].as_str(), "-X" | "--method") && !w[1].eq_ignore_ascii_case("GET"))
                || rest.iter().any(|a| {
                    a.strip_prefix("--method=")
                        .is_some_and(|m| !m.eq_ignore_ascii_case("GET"))
                        || a.strip_prefix("-X")
                            .is_some_and(|m| !m.is_empty() && !m.eq_ignore_ascii_case("GET"))
                });
            if mutating {
                Kind::Unsafe
            } else {
                Kind::Allowed
            }
        }
        ("pr", "merge" | "close" | "reopen" | "ready" | "review" | "lock" | "unlock" | "revert")
        | ("release", _)
        | ("repo", "delete" | "create" | "edit" | "fork" | "rename" | "archive" | "transfer" | "sync")
        | ("secret" | "variable" | "ssh-key" | "gpg-key" | "workflow" | "codespace" | "extension", _)
        | ("auth", _)
        | ("run", "rerun" | "cancel" | "delete")
        | ("issue", "delete" | "transfer") => Kind::Unsafe,
        // Any other gh command (update-branch, issue edit...) may write: ask.
        _ => Kind::Unsafe,
    }
}

/// `gh` accepts clusters such as `-iXDELETE`; locate the method option inside a short cluster.
fn api_short_method_is_mutating(rest: &[String]) -> bool {
    rest.iter().enumerate().any(|(i, arg)| {
        let Some(short) = arg.strip_prefix('-').filter(|s| !s.starts_with('-')) else {
            return false;
        };
        let Some(x) = short.find('X') else {
            return false;
        };
        let method = &short[x + 1..];
        if method.is_empty() {
            rest.get(i + 1).is_none_or(|m| !m.eq_ignore_ascii_case("GET"))
        } else {
            !method.eq_ignore_ascii_case("GET")
        }
    })
}

/// Largest git metadata file read for the classifier (config can be big, the rest is tiny).
const META_MAX: u64 = 1 << 20;

/// Read a regular, non-symlink file of at most `META_MAX` bytes.
fn read_small(path: &Path) -> Option<String> {
    let md = std::fs::symlink_metadata(path).ok()?;
    if !md.file_type().is_file() || md.len() > META_MAX {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The checked-out branch of the repository that contains `dir`, from `HEAD`.
fn current_branch(dir: &Path) -> Option<String> {
    let (gitdir, _) = git_dirs(dir)?;
    let head = read_small(&gitdir.join("HEAD"))?;
    head.trim().strip_prefix("ref: refs/heads/").map(str::to_string)
}

/// True only when the remote's default branch is positively known and is another branch. A
/// missing or unreadable `origin/HEAD` is unknown, which asks.
fn default_branch(dir: &Path) -> Option<String> {
    git_dirs(dir)
        .and_then(|(_, common)| read_small(&common.join("refs/remotes/origin/HEAD")))
        .and_then(|s| {
            s.trim()
                .strip_prefix("ref: refs/remotes/origin/")
                .map(str::to_string)
        })
        .filter(|d| !d.is_empty())
}

fn known_non_default(dir: &Path, branch: &str) -> bool {
    if DEFAULT_BRANCHES.contains(&branch) {
        return false;
    }
    default_branch(dir).is_some_and(|d| d != branch)
}

/// Whether the repository config can send a push somewhere the command line does not say:
/// includes, `[push]`, `remote.origin.push|pushurl|mirror`, `remote.pushDefault`, push-rewriting
/// URLs, a program-running `core.*` key, and (for an implicit push) a branch whose remote or
/// upstream is not `origin` / the same-named branch. Unreadable config is risky.
fn config_risky(dir: &Path, branch: &str, implicit: bool) -> bool {
    scan_config(dir, branch, implicit).0
}

/// The origin URLs and URL rewrites of the repo config (sorted `section.sub.key=value` lines):
/// what a push to `origin` really goes to. `None` when the config cannot be read safely.
fn url_fingerprint(dir: &Path) -> Option<Vec<String>> {
    let (risky, fp) = scan_config(dir, "", false);
    // A config that is risky for another reason still has a fingerprint; only a read failure
    // (reported as risky with an empty list) has none.
    if risky && fp.is_empty() && git_dirs(dir).is_none() {
        return None;
    }
    Some(fp)
}

/// Trusted baseline of origin's URLs per session repository (common dir), taken from the
/// first command classified for it and kept in memory for the process.
#[derive(Clone)]
struct RepoBaseline {
    urls: Vec<String>,
    default_branch: Option<String>,
}

static BASELINE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<PathBuf, RepoBaseline>>> =
    std::sync::OnceLock::new();

fn baseline_key(cwd: &Path) -> Option<PathBuf> {
    let (_, common) = git_dirs(cwd)?;
    std::fs::canonicalize(common).ok()
}

/// Record the session repository's origin URLs once, before any command of the session can
/// change them. Called for every classified command line, not only pushes.
pub(crate) fn record_baseline(cwd: &Path) {
    let Some(key) = baseline_key(cwd) else { return };
    let map = BASELINE.get_or_init(Default::default);
    let Ok(mut m) = map.lock() else { return };
    m.entry(key).or_insert_with(|| RepoBaseline {
        urls: url_fingerprint(cwd).unwrap_or_else(|| vec!["unreadable".into()]),
        default_branch: default_branch(cwd),
    });
}

/// Origin's current URLs equal the recorded baseline (an unknown baseline never matches).
fn urls_unchanged(session: &Path, target: &Path) -> bool {
    record_baseline(session);
    let Some(key) = baseline_key(session) else {
        return false;
    };
    let Some(map) = BASELINE.get() else { return false };
    let Ok(m) = map.lock() else { return false };
    match (m.get(&key), url_fingerprint(target)) {
        (Some(base), Some(now)) => base.urls == now && base.default_branch == default_branch(target),
        _ => false,
    }
}

fn scan_config(dir: &Path, branch: &str, implicit: bool) -> (bool, Vec<String>) {
    let mut fp: Vec<String> = Vec::new();
    let Some((gitdir, common)) = git_dirs(dir) else {
        return (true, fp);
    };
    if std::fs::symlink_metadata(gitdir.join("config.worktree")).is_ok() {
        return (true, fp);
    }
    let path = common.join("config");
    let text = match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        _ => match read_small(&path) {
            Some(t) => t,
            None => return (true, fp),
        },
    };
    let mut section = String::new();
    let mut sub: Option<String> = None;
    for raw in logical_lines(&text) {
        let mut line = raw.trim();
        if line.starts_with('[') {
            let Some(end) = line.find(']') else {
                return (true, fp);
            };
            let head = &line[1..end];
            let (name, s) = match head.split_once('"') {
                Some((n, rest)) => {
                    let Some(quoted) = rest.strip_suffix('"').and_then(unescape_subsection) else {
                        return (true, fp);
                    };
                    (n.trim(), Some(quoted))
                }
                None => match head.split_once('.') {
                    // Git's legacy `[section.subsection]` form lower-cases the subsection.
                    Some((n, r)) => (n, Some(r.to_ascii_lowercase())),
                    None => (head.trim(), None),
                },
            };
            section = name.to_ascii_lowercase();
            sub = s;
            line = line[end + 1..].trim();
        }
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        let (key, val) = match line.split_once('=') {
            Some((k, v)) => (
                k.trim().to_ascii_lowercase(),
                v.trim().trim_matches('"').to_string(),
            ),
            None => (line.to_ascii_lowercase(), String::new()),
        };
        let risky = match (section.as_str(), sub.as_deref()) {
            ("include" | "includeif" | "push", _) => true,
            ("remote", None) => key == "pushdefault",
            ("remote", Some("origin")) => matches!(key.as_str(), "push" | "pushurl" | "mirror"),
            ("url", _) => key == "pushinsteadof",
            ("core", _) => matches!(key.as_str(), "sshcommand" | "gitproxy" | "hookspath"),
            ("branch", Some(b)) if b == branch && implicit => match key.as_str() {
                "remote" | "pushremote" => val != "origin",
                "merge" => val != format!("refs/heads/{branch}"),
                _ => false,
            },
            _ => false,
        };
        let tracked = match (section.as_str(), sub.as_deref()) {
            ("remote", Some(_)) => matches!(key.as_str(), "url" | "pushurl" | "fetch" | "gh-resolved"),
            ("url", _) => matches!(key.as_str(), "insteadof" | "pushinsteadof"),
            _ => false,
        };
        if tracked {
            fp.push(format!("{section}.{}.{key}={val}", sub.as_deref().unwrap_or("")));
        }
        if risky {
            return (true, fp);
        }
    }
    fp.sort();
    (false, fp)
}

/// Git drops a backslash before the following character in a quoted subsection name.
fn unescape_subsection(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(chars.next()?);
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// (git dir, common dir) of the repository around `dir`; a worktree's `.git` is a file.
fn git_dirs(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let dot = d.join(".git");
        if let Ok(md) = std::fs::symlink_metadata(&dot) {
            let ft = md.file_type();
            if ft.is_symlink() {
                return None;
            }
            if ft.is_dir() {
                return Some((dot.clone(), dot));
            }
            let text = read_small(&dot)?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            let gitdir = crate::paths::normalize(Path::new(target), d);
            let common = read_small(&gitdir.join("commondir"))
                .map(|c| crate::paths::normalize(Path::new(c.trim()), &gitdir))
                .unwrap_or_else(|| gitdir.clone());
            return Some((gitdir, common));
        }
        cur = d.parent();
    }
    None
}

/// The logical lines of a git config: a line ending in an odd number of backslashes continues
/// on the next one (the backslash and the line end are dropped).
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let slashes = line.chars().rev().take_while(|c| *c == '\\').count();
        if slashes % 2 == 1 {
            buf.push_str(&line[..line.len() - 1]);
        } else {
            buf.push_str(line);
            out.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// Collect the `alias.*` names of one config file. False when the file cannot be read safely
/// or pulls in another file (an include), so the answer is unknown.
fn config_aliases(path: &Path, out: &mut Vec<String>) -> bool {
    let text = match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        _ => match read_small(path) {
            Some(t) => t,
            None => return false,
        },
    };
    let mut section = String::new();
    for raw in logical_lines(&text) {
        let mut line = raw.trim();
        if line.starts_with('[') {
            let Some(end) = line.find(']') else { return false };
            section = line[1..end]
                .split(['"', ' ', '.'])
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            line = line[end + 1..].trim();
        }
        if section.starts_with("include") {
            return false;
        }
        if section == "alias" && !line.is_empty() && !line.starts_with(['#', ';']) {
            let key = line.split('=').next().unwrap_or("").trim();
            out.push(key.to_ascii_lowercase());
        }
    }
    true
}

/// Whether git subcommand `name` may be an alias: defined by `-c alias.<name>=` on the command
/// line or in the repo, global or system config. An unreadable config counts as an alias.
fn is_alias(globals: &[String], name: &str, cwd: &Path, dirs: &[String]) -> bool {
    let name = name.to_ascii_lowercase();
    let mut k = 0;
    while k < globals.len() {
        if globals[k] == "-c" {
            if let Some(kv) = globals.get(k + 1) {
                let key = kv.split('=').next().unwrap_or("").to_ascii_lowercase();
                if key == format!("alias.{name}") {
                    return true;
                }
            }
            k += 2;
        } else {
            k += 1;
        }
    }
    let mut files: Vec<PathBuf> = Vec::new();
    let mut wd = cwd.to_path_buf();
    for d in dirs {
        wd = crate::paths::normalize(Path::new(d), &wd);
    }
    for dir in [cwd, wd.as_path()] {
        match git_dirs(dir) {
            Some((_, common)) => files.push(common.join("config")),
            None => return true,
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    if let Some(g) = std::env::var_os("GIT_CONFIG_GLOBAL") {
        files.push(PathBuf::from(g));
    }
    if let Some(h) = &home {
        files.push(h.join(".gitconfig"));
    }
    if let Some(x) = xdg {
        files.push(x.join("git/config"));
    }
    files.push(
        std::env::var_os("GIT_CONFIG_SYSTEM")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/etc/gitconfig")),
    );
    let mut names = Vec::new();
    for f in &files {
        if !config_aliases(f, &mut names) {
            return true;
        }
    }
    names.contains(&name)
}
