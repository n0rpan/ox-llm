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
    let kinds: Vec<(usize, Kind)> = {
        let mut moved = false;
        let mut out = Vec::new();
        for (i, st) in stages.iter().enumerate() {
            if st.mark.is_some() || st.words.is_empty() {
                continue;
            }
            let mut kind = kind_of(&st.words, cwd, moved);
            if matches!(first_word(&st.words), Some("cd" | "pushd" | "popd")) {
                moved = true;
            }
            if kind == Kind::Allowed
                && st
                    .redirects
                    .iter()
                    .any(|(_, t)| t != "/dev/null" && !t.starts_with('&'))
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
    if redirects
        .iter()
        .any(|(_, t)| t != "/dev/null" && !t.starts_with('&'))
    {
        return false;
    }
    if words.iter().any(|w| dynamic(w)) {
        return false;
    }
    let args: Vec<&String> = words.iter().skip_while(|w| is_assignment(w)).collect();
    match args.first().map(|w| base(w)) {
        Some("git") => git_sub(&args[1..].iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .is_some_and(|sub| GIT_LOCAL.contains(&sub.as_str())),
        Some(n) => LOCAL.contains(&n),
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
    let Some(name) = args.first().map(|w| base(w).to_string()) else {
        return Kind::Other;
    };
    match name.as_str() {
        "git" => git_kind(words, &args, cwd, moved),
        "gh" => gh_kind(words, &args),
        // A wrapper or interpreter that carries git push / gh in its words hides it.
        _ => {
            let hides = args.iter().skip(1).any(|w| {
                w == "gh" || w == "push" || w.contains("git push") || w.contains("gh ") || w.ends_with("/gh")
            });
            if hides {
                Kind::Unsafe
            } else {
                Kind::Other
            }
        }
    }
}

/// The subcommand of a git argv (after the program), skipping global options.
fn git_sub(rest: &[String]) -> Option<String> {
    let mut k = 0;
    while k < rest.len() {
        let a = rest[k].as_str();
        if matches!(a, "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace") {
            k += 2;
        } else if a.starts_with('-') {
            k += 1;
        } else {
            return Some(a.to_string());
        }
    }
    None
}

fn git_kind(all: &[String], args: &[String], cwd: &Path, moved: bool) -> Kind {
    // Find the subcommand index again to get the push arguments.
    let rest = &args[1..];
    let mut k = 0;
    let mut dir: Option<String> = None;
    let mut opaque = false;
    while k < rest.len() {
        let a = rest[k].as_str();
        if a == "-C" {
            dir = rest.get(k + 1).cloned();
            k += 2;
        } else if a.starts_with('-') {
            if matches!(a, "-c" | "--git-dir" | "--work-tree" | "--namespace")
                || a.starts_with("-c")
                || a.starts_with("--git-dir=")
                || a.starts_with("--work-tree=")
            {
                opaque = true;
            }
            k += 1;
        } else {
            break;
        }
    }
    if rest.get(k).map(String::as_str) != Some("push") {
        return Kind::Other;
    }
    let push = &rest[k + 1..];
    if opaque || moved || env_unsafe(all) || all.iter().any(|w| dynamic(w)) {
        return Kind::Unsafe;
    }
    let wd = match &dir {
        Some(d) => {
            let p = crate::paths::normalize(Path::new(d), cwd);
            if !crate::paths::is_inside(&p, cwd) {
                return Kind::Unsafe;
            }
            p
        }
        None => cwd.to_path_buf(),
    };
    let Some(current) = current_branch(&wd) else {
        return Kind::Unsafe;
    };
    if !valid_branch(&current) || is_default(&wd, &current) {
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
    let ok = match pos.as_slice() {
        [] => true,
        ["origin"] => true,
        ["origin", spec] => {
            let spec = spec.strip_prefix("HEAD:").unwrap_or(spec);
            let spec = spec.strip_prefix("refs/heads/").unwrap_or(spec);
            let spec = if let Some((src, dst)) = spec.split_once(':') {
                if src != dst && src != "HEAD" {
                    return Kind::Unsafe;
                }
                dst
            } else {
                spec
            };
            spec == "HEAD" || spec == current
        }
        _ => false,
    };
    if ok {
        Kind::Allowed
    } else {
        Kind::Unsafe
    }
}

fn valid_branch(b: &str) -> bool {
    !b.is_empty()
        && !b.starts_with(['-', '+', '/'])
        && !b.contains("..")
        && b.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

fn gh_kind(all: &[String], args: &[String]) -> Kind {
    if env_unsafe(all) || all.iter().any(|w| dynamic(w)) {
        return Kind::Unsafe;
    }
    let rest = &args[1..];
    let flags_with_value = ["-R", "--repo", "-H", "--head", "--hostname"];
    if rest.iter().any(|a| {
        flags_with_value.contains(&a.as_str())
            || a.starts_with("--repo=")
            || a.starts_with("--head=")
            || a.starts_with("-R")
    }) {
        return Kind::Unsafe;
    }
    let words: Vec<&str> = rest
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let (g, s) = (
        words.first().copied().unwrap_or(""),
        words.get(1).copied().unwrap_or(""),
    );
    match (g, s) {
        ("pr", "create" | "edit" | "comment") => {
            // Own branch's PR only: no PR argument, no body file, no other-ref flags.
            let value_flag = rest.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-F" | "--body-file" | "--template" | "-T" | "--recover"
                ) || a.starts_with("--body-file=")
            });
            // Values of flags (`--title x`) are words too; allow them when they follow a flag.
            let mut stray = false;
            let mut prev_flag = false;
            let mut seen = 0;
            for a in rest {
                if a.starts_with('-') {
                    prev_flag = !a.contains('=');
                } else {
                    if !prev_flag {
                        seen += 1;
                        if seen > 2 {
                            stray = true;
                        }
                    }
                    prev_flag = false;
                }
            }
            if value_flag || stray {
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
                matches!(a.as_str(), "-f" | "-F" | "--field" | "--raw-field" | "--input")
                    || a.starts_with("--field=")
                    || a.starts_with("--input=")
            }) || rest
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
        _ => Kind::Other,
    }
}

/// The checked-out branch of the repository that contains `dir`, from `HEAD`.
fn current_branch(dir: &Path) -> Option<String> {
    let (gitdir, _) = git_dirs(dir)?;
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    head.trim().strip_prefix("ref: refs/heads/").map(str::to_string)
}

fn is_default(dir: &Path, branch: &str) -> bool {
    if DEFAULT_BRANCHES.contains(&branch) {
        return true;
    }
    git_dirs(dir)
        .and_then(|(_, common)| std::fs::read_to_string(common.join("refs/remotes/origin/HEAD")).ok())
        .and_then(|s| {
            s.trim()
                .strip_prefix("ref: refs/remotes/origin/")
                .map(str::to_string)
        })
        .is_some_and(|d| d == branch)
}

/// (git dir, common dir) of the repository around `dir`; a worktree's `.git` is a file.
fn git_dirs(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut cur = Some(dir);
    while let Some(d) = cur {
        let dot = d.join(".git");
        if dot.is_dir() {
            return Some((dot.clone(), dot));
        }
        if dot.is_file() {
            let text = std::fs::read_to_string(&dot).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            let gitdir = crate::paths::normalize(Path::new(target), d);
            let common = std::fs::read_to_string(gitdir.join("commondir"))
                .ok()
                .map(|c| crate::paths::normalize(Path::new(c.trim()), &gitdir))
                .unwrap_or_else(|| gitdir.clone());
            return Some((gitdir, common));
        }
        cur = d.parent();
    }
    None
}
