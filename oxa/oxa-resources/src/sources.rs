//! E10: package source strings and resource filters.
// ported from pi packages/coding-agent/src/core/package-manager.ts (source parsing, filters)
use oxa_proto::{codes, OxaError};
use std::path::{Path, PathBuf};

/// A parsed package source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PackageSource {
    /// `git:<host>/<path>[@<ref>]`, or an `https://` / `ssh://` / `git@` URL.
    Git { url: String, reference: Option<String> },
    /// A local path: loaded in place, never copied. Relative paths resolve from the directory
    /// of the settings file that declares them.
    Local(PathBuf),
}

impl PackageSource {
    /// Stable identity used for de-duplication and for the install directory name: the source
    /// without its version or ref.
    pub fn identity(&self) -> String {
        match self {
            PackageSource::Git { url, .. } => format!("git:{}", git_path(url)),
            PackageSource::Local(path) => format!("local:{}", path.display()),
        }
    }

    /// `true` when the version / ref is exact (an update never moves it). A local path has no
    /// version and an update never touches it, so it counts as pinned.
    pub fn pinned(&self) -> bool {
        match self {
            PackageSource::Git { reference, .. } => reference.is_some(),
            PackageSource::Local(_) => true,
        }
    }
}

/// The host/path part of a git URL, the way it names the install directory: no scheme, no
/// user, `:` of scp-like URLs turned into `/`, no trailing `.git` or slashes.
pub(crate) fn git_path(url: &str) -> String {
    let mut rest = url;
    for scheme in ["https://", "http://", "ssh://", "git://", "file://"] {
        if let Some(r) = rest.strip_prefix(scheme) {
            rest = r;
            break;
        }
    }
    if let Some(at) = rest.find('@') {
        if rest.find('/').is_none_or(|slash| at < slash) {
            rest = &rest[at + 1..];
        }
    }
    let rest = rest.replacen(':', "/", 1);
    let rest = rest.trim_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    rest.trim_matches('/').to_string()
}

/// Parse a source string. `base` resolves relative local paths. Errors: `invalid` with a hint
/// listing the accepted forms; `unsupported_in_oxa` for an `npm:` source (O-175).
pub fn parse_source(s: &str, base: &Path) -> Result<PackageSource, OxaError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(invalid_source(s));
    }
    if s == "self" || s == "pi" {
        return Err(
            OxaError::unsupported(&format!("updating oxa itself (`oxa update {s}`)"))
                .with_hint("Update oxa by installing a new build; `oxa update` only updates packages."),
        );
    }
    if s.starts_with("npm:") {
        return Err(OxaError::new(
            codes::UNSUPPORTED_IN_OXA,
            "npm package sources are not supported; use a git URL or a local path",
        ));
    }
    if let Some(rest) = s.strip_prefix("git:") {
        // `git:host/path[@ref]` is the short form of `https://host/path[@ref]`.
        let (path, reference) = split_ref(rest);
        if !path.contains('/') {
            return Err(invalid_source(s));
        }
        let url = format!("https://{path}");
        check_git_components(&url, s)?;
        return Ok(PackageSource::Git {
            url,
            reference: reference.map(str::to_string),
        });
    }
    if is_git_url(s) {
        let (url, reference) = match s.rfind('@') {
            Some(i) if i > 0 => {
                let cand = &s[i + 1..];
                if !cand.is_empty() && !cand.contains(['/', ':']) {
                    (&s[..i], Some(cand.to_string()))
                } else {
                    (s, None)
                }
            }
            _ => (s, None),
        };
        check_git_components(url, s)?;
        return Ok(PackageSource::Git {
            url: url.to_string(),
            reference,
        });
    }
    if is_path_like(s) {
        let p = Path::new(s);
        let path = if p.is_absolute() {
            p.to_path_buf()
        } else {
            base.join(p)
        };
        return Ok(PackageSource::Local(path));
    }
    Err(invalid_source(s))
}

fn invalid_source(s: &str) -> OxaError {
    OxaError::new(codes::INVALID, format!("invalid package source: {s:?}"))
        .with_hint("Accepted forms: git:<host>/<path>[@ref], https://, ssh:// or git@ URLs, or a local path.")
}

fn is_git_url(s: &str) -> bool {
    ["https://", "http://", "ssh://", "git://", "file://", "git@"]
        .iter()
        .any(|p| s.starts_with(p))
}

fn is_path_like(s: &str) -> bool {
    s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('~')
        || s == "."
        || s == ".."
        || s.contains('/')
        || s.contains('\\')
}

/// Split a trailing `@ref` off a `host/path` form (the ref never contains `/`).
fn split_ref(rest: &str) -> (&str, Option<&str>) {
    match rest.rfind('@') {
        Some(i) if i > 0 => {
            let cand = &rest[i + 1..];
            if !cand.is_empty() && !cand.contains(['/', ':']) {
                (&rest[..i], Some(cand))
            } else {
                (rest, None)
            }
        }
        _ => (rest, None),
    }
}

/// Refuse URLs whose install path would escape the packages directory.
fn check_git_components(url: &str, s: &str) -> Result<(), OxaError> {
    let path = git_path(url);
    if path.is_empty() || path.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(invalid_source(s));
    }
    Ok(())
}

/// Apply a package resource filter to the paths a package declares.
///
/// `all` are package-relative paths; `filter` is the settings list: `None` = everything,
/// `Some([])` = nothing, a plain entry = include glob, `!pattern` = exclude glob, `+path` =
/// include one exact path, `-path` = exclude one exact path. Filters only narrow: a path not in
/// `all` never appears. The result keeps the order of `all`.
///
/// Steps as pi's `applyPatterns`: includes (all paths when there are none), then excludes, then
/// force-includes (back from `all`), then force-excludes. A plain entry without glob characters
/// also matches the paths under it (`skills/x` selects `skills/x/SKILL.md`).
pub fn apply_filter(all: &[String], filter: Option<&[String]>) -> Vec<String> {
    let Some(entries) = filter else {
        return all.to_vec();
    };
    if entries.is_empty() {
        return Vec::new();
    }
    let mut includes = Vec::new();
    let mut excludes = Vec::new();
    let mut force_in = Vec::new();
    let mut force_out = Vec::new();
    for e in entries {
        if let Some(p) = e.strip_prefix('+') {
            force_in.push(p);
        } else if let Some(p) = e.strip_prefix('-') {
            force_out.push(p);
        } else if let Some(p) = e.strip_prefix('!') {
            excludes.push(p);
        } else {
            includes.push(e.as_str());
        }
    }
    all.iter()
        .filter(|path| {
            let mut keep = includes.is_empty() || includes.iter().any(|p| pattern_hits(p, path));
            if excludes.iter().any(|p| pattern_hits(p, path)) {
                keep = false;
            }
            if force_in.iter().any(|p| exact_hits(p, path)) {
                keep = true;
            }
            if force_out.iter().any(|p| exact_hits(p, path)) {
                keep = false;
            }
            keep
        })
        .cloned()
        .collect()
}

/// A glob matched against the whole path or its file name; a plain name also matches the paths
/// under it.
fn pattern_hits(pattern: &str, path: &str) -> bool {
    let pattern = normalize_pattern(pattern);
    let name = path.rsplit('/').next().unwrap_or(path);
    let bare = !pattern.contains('/');
    if has_glob(&pattern) {
        glob_match(&pattern, path) || (bare && glob_match(&pattern, name))
    } else {
        exact_hits(&pattern, path) || (bare && name == pattern)
    }
}

/// An exact path, or a directory prefix of it.
fn exact_hits(pattern: &str, path: &str) -> bool {
    let pattern = normalize_pattern(pattern);
    path == pattern
        || path
            .strip_prefix(pattern.as_str())
            .is_some_and(|r| r.starts_with('/'))
}

pub(crate) fn normalize_pattern(p: &str) -> String {
    let mut s = p.trim();
    while let Some(r) = s.strip_prefix("./") {
        s = r;
    }
    s.trim_start_matches('/').trim_end_matches('/').to_string()
}

pub(crate) fn has_glob(p: &str) -> bool {
    p.contains('*') || p.contains('?')
}

/// `*` (no `/`), `?` (one character, no `/`) and `**` (any number of path segments).
pub(crate) fn glob_match(pattern: &str, path: &str) -> bool {
    let pats: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = path.split('/').collect();
    match_segments(&pats, &segs)
}

fn match_segments(pats: &[&str], segs: &[&str]) -> bool {
    match pats.first() {
        None => segs.is_empty(),
        Some(&"**") => (0..=segs.len()).any(|skip| match_segments(&pats[1..], &segs[skip..])),
        Some(p) => match segs.first() {
            None => false,
            Some(s) => match_one(p.as_bytes(), s.as_bytes()) && match_segments(&pats[1..], &segs[1..]),
        },
    }
}

fn match_one(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') => (0..=s.len()).any(|k| match_one(&p[1..], &s[k..])),
        Some(b'?') => !s.is_empty() && match_one(&p[1..], &s[1..]),
        Some(c) => s.first() == Some(c) && match_one(&p[1..], &s[1..]),
    }
}
