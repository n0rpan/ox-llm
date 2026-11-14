//! sbx-git: the git data a repository keeps outside the cwd, found by reading files only.
//!
//! A git worktree keeps its `.git` data in the main repository (`<main>/.git/worktrees/<name>`
//! and the common dir `<main>/.git`); a shared clone (`git clone --shared`) reads objects from
//! the directories listed in `<gitdir>/objects/info/alternates`. The sandbox binds only the cwd,
//! so git fails there unless these directories are bound too.
//!
//! [`resolve`] runs once, when the session's sandbox is built (`Bwrap::with_git_dirs`), and
//! never runs `git` on the repository (a repository's config must not get to execute anything;
//! sbx-gitsym's cross-check only has `git config --file` parse one config file). The `.git` file
//! and the alternates are repository content, so a candidate is bound only when it looks like
//! git data and points back at us:
//!
//! * a `.git` *file* (`gitdir: <path>`) needs the gitdir's back-pointer `gitdir` file, which
//!   must resolve to that `.git` file, and its `commondir` must hold `worktrees/<name>` pointing
//!   back at the gitdir;
//! * a gitdir needs a `HEAD` file and an `objects` directory (its own or its common dir's);
//! * an alternate must be a directory named `objects`, or holding `pack` or two-hex-digit
//!   directories, and it is followed at most [`MAX_ALTERNATES_DEPTH`] levels deep (no cycles);
//! * nothing that is, contains or lies inside the deny-list ([`sensitive_paths`]), and never
//!   `$HOME` or a directory above it, is bound.
//!
//! Anything that fails is dropped: git then fails as it does today, and bash keeps working.
//!
//! sbx-hooks: the entries that git runs or reads outside the sandbox later
//! ([`protected_entries`]: `hooks` and `config` of the common dir, `config.worktree` of the
//! common dir and of a worktree's gitdir when `extensions.worktreeConfig` may be on) stay as
//! they are in a writable sandbox, also when they do not exist yet. An existing entry is bound
//! read-only; a missing `hooks` is masked with an empty read-only tmpfs, and a missing file is
//! created empty on the host just before the command and bound read-only (a `/dev/null` mask
//! cannot be read on a read-only bind, and git refuses an unreadable `config`), so the command
//! cannot create either. bwrap creates the tmpfs's mount point in the read-write git
//! directory, which is the host's: after the first command an empty `hooks` directory, or an
//! empty `config` / `config.worktree` file, is left in the repository. git
//! reads an empty one as no hooks and no settings, and the next command binds it read-only as
//! an existing entry. Removing it after the run is not sound: another command running at the
//! same time would lose its mask (unlinking a mount point that lives in another mount
//! namespace detaches it there). A symlink is never bound over, and a missing entry is masked
//! only inside a real directory.
//!
//! sbx-gitsym: a read-only bind protects a path, not what it names. A protected entry that is
//! a symlink lives in the read-write directory itself, so a command can replace the link (or
//! rename a directory on the way to its target, or write the target through another name) and
//! git outside the sandbox then reads the command's config or runs its hooks. Binding the
//! canonical target read-only does not close that, so a git directory whose protected entries
//! are not plain ([`unsafe_reason`]: a symlink, a hard-linked file, a symlink or hard link
//! inside `hooks`, a config that includes other files, anything but a regular file or real
//! directory) is never exposed read-write: [`resolve`] binds it read-only with a notice, and
//! `bwrap.rs` checks again for every command (also the cwd's own `.git`). Git commands that
//! write then fail in that repository; reading works.
//!
//! Read-only git directories do not cover an alias whose target lies elsewhere: a symlink or an
//! `include` can name a file in the writable cwd or a writable cache. So for such a repository
//! every alias is followed ([`alias_refusal`]): each hop of every symlink chain, each directory
//! on the way, every `include.path` / `includeIf.*.path` (nested, at most
//! [`MAX_INCLUDE_DEPTH`]) and everything below `hooks`. When one of them reaches a path the
//! command could write, cannot be followed (dangling, a loop, too many hops, an unknown include
//! form), or is a file with more than one hard link (its other names are unknown), the sandbox
//! refuses to run commands in that repository (`bwrap.rs`).
use crate::layout::{is_home_or_above, sensitive_paths};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// How many levels of `objects/info/alternates` are followed.
pub const MAX_ALTERNATES_DEPTH: usize = 5;

/// sbx-gitsym: at most this many entries of a `hooks` directory are checked; more is unsafe.
const MAX_HOOKS_ENTRIES: usize = 4096;

/// sbx-gitsym: how many levels of config `include`s are followed; deeper is refused.
pub const MAX_INCLUDE_DEPTH: usize = 5;

/// sbx-gitsym: symlinks followed while resolving one alias (Linux allows 40); more is refused.
const MAX_SYMLINK_HOPS: usize = 40;

/// The extra binds for the git data of the cwd. Every path is canonical, absolute and outside
/// the bound cwd. The deny-list masks are applied after these binds (`bwrap.rs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitBinds {
    /// Read-write (read-only when the command may not write): a worktree's gitdir and the
    /// common dir of the repository (commits, refs, index).
    pub rw: Vec<PathBuf>,
    /// Read-only: alternate object directories (`objects` themselves).
    pub ro: Vec<PathBuf>,
    /// Read-only, inside the read-write dirs: the hooks and config files that git runs or reads
    /// outside the sandbox later (`hooks`, `config`, `config.worktree`). An entry that does not
    /// exist ([`protected_entries`]) is masked instead in a writable sandbox (`bwrap.rs`).
    pub protect: Vec<PathBuf>,
    /// sbx-gitsym: why a git directory of the cwd is bound read-only although the command may
    /// write ([`unsafe_reason`]), one line each, for the session's notices. Binds nothing.
    pub notes: Vec<String>,
    /// sbx-gitsym: the repository's git directories (the gitdir and the common dir, canonical,
    /// also when they lie in the cwd), for the per-command alias check. Binds nothing.
    pub dirs: Vec<PathBuf>,
}

impl GitBinds {
    /// Every path this bind set covers.
    pub fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.rw.iter().chain(&self.ro).chain(&self.protect)
    }

    /// `true` when nothing is bound beyond the cwd.
    pub fn is_empty(&self) -> bool {
        self.rw.is_empty() && self.ro.is_empty() && self.protect.is_empty()
    }
}

/// The git binds for `cwd`, a `$HOME` of `home` and the agent directory `agent_dir`. Pure over
/// the file system state; `cwd` is the session's working directory.
pub fn resolve(cwd: &Path, home: &Path, agent_dir: Option<&Path>) -> GitBinds {
    let Some(cwd) = canon(cwd) else {
        return GitBinds::default();
    };
    let home_c = canon(home).unwrap_or_else(|| home.to_path_buf());
    let ctx = Ctx {
        home: home_c.clone(),
        deny: deny_list(home, &home_c, agent_dir),
    };
    let Some(dotgit) = find_dotgit(&cwd, &ctx.home) else {
        return GitBinds::default();
    };
    let Some((gitdir, common)) = locate(&dotgit, &ctx) else {
        return GitBinds::default();
    };
    let objects = common.join("objects");
    let mut alternates = Vec::new();
    let mut seen = BTreeSet::new();
    seen.insert(objects.clone());
    collect_alternates(&objects, 1, &ctx, &mut seen, &mut alternates);

    let outside = |p: &PathBuf| !p.starts_with(&cwd) && !cwd.starts_with(p);
    let mut rw: Vec<PathBuf> = Vec::new();
    for d in [&gitdir, &common] {
        if outside(d) && !rw.contains(d) {
            rw.push(d.clone());
        }
    }
    let mut ro: Vec<PathBuf> = alternates.into_iter().filter(outside).collect();
    // sbx-gitsym: a git directory with a protected entry that is not plain is never read-write
    // (also the cwd's own `.git`, which `bwrap.rs` binds read-only then); one unsafe directory
    // makes every git directory of the repository read-only (a commit needs both).
    let mut notes: Vec<String> = Vec::new();
    for d in [&gitdir, &common] {
        if let Some(why) = unsafe_reason(d) {
            let line = format!(
                "git: {why}; the repository's git data is read-only in the sandbox (git commands that write will fail)"
            );
            if !notes.contains(&line) {
                notes.push(line);
            }
        }
    }
    if !notes.is_empty() {
        ro.append(&mut rw);
    }
    let mut protect: Vec<PathBuf> = Vec::new();
    for d in &rw {
        for rel in ["config", "config.worktree", "hooks"] {
            let p = d.join(rel);
            if fs::symlink_metadata(&p).is_ok_and(|m| !m.file_type().is_symlink()) {
                protect.push(p);
            }
        }
        protect.extend(protected_entries(d, &common));
    }
    rw.sort();
    ro.sort();
    ro.dedup();
    protect.sort();
    protect.dedup();
    let mut dirs = vec![gitdir.clone(), common.clone()];
    dirs.dedup();
    GitBinds {
        rw,
        ro,
        protect,
        notes,
        dirs,
    }
}

/// sbx-gitsym: why the git directory `dir` must not be exposed read-write, or `None` when its
/// protected entries (`config`, `config.worktree`, `hooks`; each may be missing) are plain: each
/// file a regular file with one link and no `include`, `hooks` a real directory whose entries
/// (at most [`MAX_HOOKS_ENTRIES`], walked without following links) are real directories or
/// regular files with one link. Anything that cannot be checked is unsafe. Every check is on
/// the entry itself, so symlink chains, `..` in a target and a symlinked `hooks` directory are
/// all refused, whatever they point at.
pub(crate) fn unsafe_reason(dir: &Path) -> Option<String> {
    for name in ["config", "config.worktree"] {
        let p = dir.join(name);
        let m = match fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Some(format!("{} cannot be checked ({e})", p.display())),
        };
        if let Some(why) = not_plain_file(&p, &m) {
            return Some(why);
        }
        match config_includes(&p, &common_of(dir)) {
            Ok(inc) if inc.header || !inc.paths.is_empty() => {
                return Some(format!("{} includes other config files", p.display()));
            }
            Ok(_) => {}
            Err(why) => return Some(why),
        }
    }
    let hooks = dir.join("hooks");
    match fs::symlink_metadata(&hooks) {
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => Some(format!("{} cannot be checked ({e})", hooks.display())),
        Ok(m) if m.file_type().is_symlink() => Some(format!("{} is a symlink", hooks.display())),
        Ok(m) if !m.is_dir() => Some(format!("{} is not a directory", hooks.display())),
        Ok(_) => unsafe_hooks_entry(&hooks),
    }
}

/// A protected file that is not a regular file with one link (a symlink, a hard link that
/// another name can write, a device or directory).
fn not_plain_file(p: &Path, m: &fs::Metadata) -> Option<String> {
    if m.file_type().is_symlink() {
        Some(format!("{} is a symlink", p.display()))
    } else if !m.is_file() {
        Some(format!("{} is not a regular file", p.display()))
    } else if m.nlink() > 1 {
        Some(format!("{} has more than one hard link", p.display()))
    } else {
        None
    }
}

/// The first entry below the real directory `hooks` that is not plain, walked without
/// following links.
fn unsafe_hooks_entry(hooks: &Path) -> Option<String> {
    let mut stack = vec![hooks.to_path_buf()];
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        let entries = match fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) => return Some(format!("{} cannot be read ({e})", d.display())),
        };
        for entry in entries {
            let p = match entry {
                Ok(e) => e.path(),
                Err(e) => return Some(format!("{} cannot be read ({e})", d.display())),
            };
            seen += 1;
            if seen > MAX_HOOKS_ENTRIES {
                return Some(format!("{} has too many entries to check", hooks.display()));
            }
            let m = match fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(e) => return Some(format!("{} cannot be checked ({e})", p.display())),
            };
            if m.is_dir() {
                stack.push(p);
            } else if let Some(why) = not_plain_file(&p, &m) {
                return Some(why);
            }
        }
    }
    None
}

/// sbx-hooks: the entries of the git directory `dir` (whose common dir is `common`) that git
/// runs or reads outside the sandbox later, existing or missing: `config` and `hooks` when `dir`
/// is the common dir (git reads hooks from the common dir only), and `config.worktree` when
/// `extensions.worktreeConfig` may be on in the common dir's `config`. Left out: an entry that
/// is a symlink (never bound over), and every entry when `dir` is not a real directory. Sorted.
pub(crate) fn protected_entries(dir: &Path, common: &Path) -> Vec<PathBuf> {
    if !is_real_dir(dir) {
        return Vec::new();
    }
    let mut names: Vec<&str> = Vec::new();
    if dir == common {
        names.extend(["config", "hooks"]);
    }
    if worktree_config_may_be_on(common) {
        names.push("config.worktree");
    }
    names.sort_unstable();
    names
        .into_iter()
        .map(|n| dir.join(n))
        .filter(|p| match fs::symlink_metadata(p) {
            Ok(m) => !m.file_type().is_symlink(),
            Err(e) => e.kind() == ErrorKind::NotFound,
        })
        .collect()
}

/// `extensions.worktreeConfig` may be on: the common dir's `config` names it (any case, any
/// value), or the file exists but cannot be read. A missing `config` turns nothing on (and it is
/// masked itself, so the command cannot write one).
fn worktree_config_may_be_on(common: &Path) -> bool {
    match fs::read(common.join("config")) {
        Ok(bytes) => String::from_utf8_lossy(&bytes)
            .to_ascii_lowercase()
            .contains("worktreeconfig"),
        Err(e) => e.kind() != ErrorKind::NotFound,
    }
}

/// A real directory (not a symlink to one).
pub(crate) fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// What the candidates are checked against.
struct Ctx {
    /// `$HOME`, canonical.
    home: PathBuf,
    /// The deny-list, raw and canonical.
    deny: Vec<PathBuf>,
}

impl Ctx {
    /// A canonical candidate may be bound: not `$HOME` or above it, not a deny-listed path,
    /// not a directory that contains one.
    fn usable(&self, p: &Path) -> bool {
        !is_home_or_above(p, &self.home) && !self.deny.iter().any(|d| p.starts_with(d) || d.starts_with(p))
    }
}

fn deny_list(home: &Path, home_c: &Path, agent_dir: Option<&Path>) -> Vec<PathBuf> {
    let agent_c = agent_dir.map(|a| canon(a).unwrap_or_else(|| a.to_path_buf()));
    let mut out = Vec::new();
    for raw in sensitive_paths(home, agent_dir)
        .into_iter()
        .chain(sensitive_paths(home_c, agent_c.as_deref()))
    {
        if let Some(c) = canon(&raw) {
            out.push(c);
        }
        out.push(raw);
    }
    out.sort();
    out.dedup();
    out
}

/// The first `.git` entry (file or directory, not followed) from `cwd` upward, stopping at
/// `$HOME` and above.
fn find_dotgit(cwd: &Path, home: &Path) -> Option<PathBuf> {
    let mut dir = cwd;
    loop {
        if is_home_or_above(dir, home) {
            return None;
        }
        let dotgit = dir.join(".git");
        if fs::symlink_metadata(&dotgit).is_ok() {
            return Some(dotgit);
        }
        dir = dir.parent()?;
    }
}

/// The gitdir and the common dir behind a `.git` entry, when both look right.
fn locate(dotgit: &Path, ctx: &Ctx) -> Option<(PathBuf, PathBuf)> {
    let meta = fs::symlink_metadata(dotgit).ok()?;
    if meta.is_dir() {
        // A plain repository: `.git` is its own gitdir and common dir.
        let gitdir = canon(dotgit)?;
        if gitdir.join("commondir").exists() || !is_gitdir(&gitdir, &gitdir) {
            return None;
        }
        return usable_pair(&gitdir, &gitdir, ctx);
    }
    if !meta.is_file() {
        return None;
    }
    // A worktree: `.git` is a file naming the gitdir.
    let text = fs::read_to_string(dotgit).ok()?;
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))?
        .trim();
    let top = dotgit.parent()?;
    let gitdir = canon(&resolve_rel(top, line))?;
    if !gitdir.is_dir() {
        return None;
    }
    // git's own back-pointer: the gitdir names the `.git` file we came from.
    let back = read_trim(&gitdir.join("gitdir"))?;
    if canon(&resolve_rel(&gitdir, &back))? != canon(dotgit)? {
        return None;
    }
    let common = canon(&resolve_rel(&gitdir, &read_trim(&gitdir.join("commondir"))?))?;
    if !common.is_dir() {
        return None;
    }
    let name = gitdir.file_name()?;
    if canon(&common.join("worktrees").join(name))? != gitdir {
        return None;
    }
    if !is_gitdir(&gitdir, &common) {
        return None;
    }
    usable_pair(&gitdir, &common, ctx)
}

fn usable_pair(gitdir: &Path, common: &Path, ctx: &Ctx) -> Option<(PathBuf, PathBuf)> {
    (ctx.usable(gitdir) && ctx.usable(common)).then(|| (gitdir.to_path_buf(), common.to_path_buf()))
}

/// A gitdir has a `HEAD` file, and an `objects` directory in itself or in its common dir.
fn is_gitdir(gitdir: &Path, common: &Path) -> bool {
    gitdir.join("HEAD").is_file() && (gitdir.join("objects").is_dir() || common.join("objects").is_dir())
}

/// Follow `info/alternates` of `objects` (levels 1 to [`MAX_ALTERNATES_DEPTH`]); each usable
/// object directory is added once, in the order listed.
fn collect_alternates(
    objects: &Path,
    depth: usize,
    ctx: &Ctx,
    seen: &mut BTreeSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) {
    if depth > MAX_ALTERNATES_DEPTH {
        return;
    }
    let Ok(text) = fs::read_to_string(objects.join("info").join("alternates")) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(alt) = canon(&resolve_rel(objects, line)) else {
            continue;
        };
        if !seen.insert(alt.clone()) {
            continue;
        }
        if !is_objects_dir(&alt) || !ctx.usable(&alt) {
            continue;
        }
        out.push(alt.clone());
        collect_alternates(&alt, depth + 1, ctx, seen, out);
    }
}

/// An object directory: named `objects`, or holding `pack` or two-hex-digit directories.
fn is_objects_dir(p: &Path) -> bool {
    if !p.is_dir() {
        return false;
    }
    p.file_name().is_some_and(|n| n == "objects") || p.join("pack").is_dir() || has_fanout_dir(p)
}

fn has_fanout_dir(p: &Path) -> bool {
    let Ok(entries) = fs::read_dir(p) else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name.len() == 2 && name.chars().all(|c| c.is_ascii_hexdigit()) && e.path().is_dir()
    })
}

/// `raw` as a path: absolute as it is, relative to `base` otherwise.
fn resolve_rel(base: &Path, raw: &str) -> PathBuf {
    let p = Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

fn read_trim(p: &Path) -> Option<String> {
    let s = fs::read_to_string(p).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn canon(p: &Path) -> Option<PathBuf> {
    fs::canonicalize(p).ok()
}

/// sbx-gitsym: what a git config file says about `include`s, read as git reads it
/// (`config.c`: `parse_file`, `get_base_var`, `get_value`, `parse_value`): whether it has a
/// section named `include` / `includeIf` (any subsection, also the legacy dotted form; this
/// only makes the repository read-only), and the exact bytes of every value that git treats
/// as an include (`include.path`, `includeif.<condition>.path`), in file order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Includes {
    pub header: bool,
    pub paths: Vec<Vec<u8>>,
    /// Every `core.hooksPath` value, in file order (git uses the last one). The sandbox
    /// protects the directory a repository config names: git runs the hooks there outside
    /// the sandbox later.
    pub hooks_paths: Vec<Vec<u8>>,
}

/// A byte stream with git's `get_next_char`: CR LF reads as LF.
struct Stream<'a> {
    b: &'a [u8],
    i: usize,
}

impl Stream<'_> {
    /// The next byte, `None` at the end (git: a final `\n` and `eof`).
    fn next(&mut self) -> Option<u8> {
        let c = *self.b.get(self.i)?;
        self.i += 1;
        if c == b'\r' && self.b.get(self.i) == Some(&b'\n') {
            self.i += 1;
            return Some(b'\n');
        }
        Some(c)
    }

    /// The next byte, `\n` at the end (git's `get_next_char` after `eof`).
    fn next_or_nl(&mut self) -> u8 {
        self.next().unwrap_or(b'\n')
    }
}

/// git's `isspace` (sane-ctype): space, tab, LF, CR.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// Parse `bytes` as a git config file. Anything git would reject (a bad header, a bad key
/// line, an unterminated quote, an unknown escape, an include without a value) or that
/// cannot be compared byte for byte (a NUL) is an error: the caller refuses, it never guesses.
pub(crate) fn parse_includes(bytes: &[u8]) -> Result<Includes, String> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    if bytes.contains(&0) {
        return Err("a NUL byte in the config".into());
    }
    let mut s = Stream { b: bytes, i: 0 };
    let mut out = Includes::default();
    let mut section: Vec<u8> = Vec::new();
    let mut comment = false;
    while let Some(c) = s.next() {
        if c == b'\n' {
            comment = false;
            continue;
        }
        if comment || is_space(c) {
            continue;
        }
        if c == b'#' || c == b';' {
            comment = true;
            continue;
        }
        if c == b'[' {
            section = section_header(&mut s)?;
            let base = section.split(|b| *b == b'.').next().unwrap_or(&[]);
            out.header |= base == b"include" || base == b"includeif";
            continue;
        }
        if !c.is_ascii_alphabetic() {
            return Err("a config line git rejects".into());
        }
        let mut key = vec![c.to_ascii_lowercase()];
        let mut c = s.next_or_nl();
        while c.is_ascii_alphanumeric() || c == b'-' {
            key.push(c.to_ascii_lowercase());
            c = s.next_or_nl();
        }
        while c == b' ' || c == b'\t' {
            c = s.next_or_nl();
        }
        let value = match c {
            b'\n' => None,
            b'=' => Some(parse_value(&mut s)?),
            _ => return Err("a config line git rejects".into()),
        };
        let include = section == b"include"
            || (section.starts_with(b"includeif.") && section.len() > b"includeif.".len());
        if include && key == b"path" {
            match value {
                Some(v) if !v.is_empty() => out.paths.push(v),
                _ => return Err("an include path without a value".into()),
            }
        } else if section == b"core" && key == b"hookspath" {
            if let Some(v) = value {
                if !v.is_empty() {
                    out.hooks_paths.push(v);
                }
            }
        }
    }
    Ok(out)
}

/// git's `get_base_var` after `[`: the section name lowercased, then `.` and the subsection as
/// written (`[name "sub"]`, `\` takes the next byte) or the legacy dotted form (`[name.sub]`).
fn section_header(s: &mut Stream<'_>) -> Result<Vec<u8>, String> {
    let bad = || "a section header git rejects".to_string();
    let mut name = Vec::new();
    loop {
        let c = s.next().ok_or_else(bad)?;
        if c == b']' {
            return Ok(name);
        }
        if is_space(c) {
            break;
        }
        if !(c.is_ascii_alphanumeric() || c == b'-' || c == b'.') {
            return Err(bad());
        }
        name.push(c.to_ascii_lowercase());
    }
    // `get_extended_base_var`: spaces, then a quoted subsection, then `]`.
    let mut c = s.next().ok_or_else(bad)?;
    while is_space(c) {
        c = s.next().ok_or_else(bad)?;
    }
    if c != b'"' {
        return Err(bad());
    }
    name.push(b'.');
    loop {
        let mut c = s.next().ok_or_else(bad)?;
        if c == b'\n' {
            return Err(bad());
        }
        if c == b'"' {
            break;
        }
        if c == b'\\' {
            c = s.next().ok_or_else(bad)?;
            if c == b'\n' {
                return Err(bad());
            }
        }
        name.push(c);
    }
    if s.next() != Some(b']') {
        return Err(bad());
    }
    Ok(name)
}

/// git's `parse_value` after `=`: whitespace inside quotes is kept, outside them leading
/// whitespace is dropped and trailing whitespace trimmed (inner kept), `#` / `;` outside quotes
/// start a comment, the escapes are `\\` `\"` `\n` `\t` `\b`, a `\` at the end of a line
/// continues the value, a quote still open at the end of the line is an error, and so is any
/// other escape.
fn parse_value(s: &mut Stream<'_>) -> Result<Vec<u8>, String> {
    let mut v: Vec<u8> = Vec::new();
    let mut quote = false;
    let mut comment = false;
    let mut trim_len = 0usize;
    loop {
        let c = s.next_or_nl();
        if c == b'\n' {
            if quote {
                return Err("an unterminated quote in a config value".into());
            }
            if trim_len > 0 {
                v.truncate(trim_len);
            }
            return Ok(v);
        }
        if comment {
            continue;
        }
        if is_space(c) && !quote {
            if trim_len == 0 {
                trim_len = v.len();
            }
            if !v.is_empty() {
                v.push(c);
            }
            continue;
        }
        if !quote && (c == b';' || c == b'#') {
            comment = true;
            continue;
        }
        trim_len = 0;
        match c {
            b'\\' => match s.next_or_nl() {
                b'\n' => {}
                b't' => v.push(b'\t'),
                b'b' => v.push(b'\x08'),
                b'n' => v.push(b'\n'),
                d @ (b'\\' | b'"') => v.push(d),
                _ => return Err("an escape git rejects in a config value".into()),
            },
            b'"' => quote = !quote,
            _ => v.push(c),
        }
    }
}

/// sbx-gitsym: the includes of the config file `p`, parsed ([`parse_includes`]) and, when git
/// can be run, cross-checked: `git config --file <p> --no-includes --null --get-regexp` lists
/// the include values git itself reads from that one file (no repository, no includes
/// followed, no global or system config, no network, at most [`GIT_CHECK_TIMEOUT`]); any
/// difference from the parser is an error. No git on `PATH` leaves the parser alone.
fn config_includes(p: &Path, repo: &Path) -> Result<Includes, String> {
    let bytes = fs::read(p).map_err(|e| format!("{} cannot be read ({e})", p.display()))?;
    let inc = parse_includes(&bytes).map_err(|e| format!("{}: {e}", p.display()))?;
    if let Some(git) = git_include_values(p, repo) {
        let git = git.map_err(|e| format!("{}: {e}", p.display()))?;
        if git != inc.paths {
            return Err(format!(
                "{}: git reads its includes differently from the sandbox's check",
                p.display()
            ));
        }
    }
    Ok(inc)
}

/// sbx-hooks: the `core.hooksPath` values of `dir`'s config and `config.worktree`, including
/// the values in files they include (all `includeIf` conditions count: this only protects
/// more). A value is returned as git wrote it; the caller resolves it against the worktree
/// root (`cwd`). Unreadable or unparsable files are skipped here: [`unsafe_reason`] and
/// [`alias_refusal`] already refuse a git directory whose config cannot be checked.
pub(crate) fn configured_hooks_paths(dir: &Path, home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for name in ["config", "config.worktree"] {
        read_hooks_paths(&dir.join(name), home, 0, &mut seen, &mut out);
    }
    out
}

fn read_hooks_paths(
    path: &Path,
    home: &Path,
    depth: usize,
    seen: &mut BTreeSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) {
    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    let Ok(real) = fs::canonicalize(path) else {
        return;
    };
    if !seen.insert(real.clone()) {
        return;
    }
    let Ok(bytes) = fs::read(&real) else {
        return;
    };
    let Ok(inc) = parse_includes(&bytes) else {
        return;
    };
    for raw in inc.hooks_paths {
        out.push(PathBuf::from(OsString::from_vec(raw)));
    }
    for raw in inc.paths {
        let raw = PathBuf::from(OsString::from_vec(raw));
        let p = if raw == Path::new("~") {
            home.to_path_buf()
        } else if let Some(rest) = raw.to_str().and_then(|s| s.strip_prefix("~/")) {
            home.join(rest)
        } else if raw.is_absolute() {
            raw
        } else {
            real.parent().unwrap_or(Path::new(".")).join(raw)
        };
        read_hooks_paths(&p, home, depth + 1, seen, out);
    }
}

/// sbx-gitsym: how long the `git config` cross-check may take.
const GIT_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// sbx-gitsym: after a `git config` check of a repository did not finish within
/// [`GIT_CHECK_TIMEOUT`], that repository's checks fail at once for this long; then one fresh
/// bounded attempt is made. Other repositories are not affected.
const GIT_CHECK_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

/// sbx-gitsym: per repository (its canonical common dir), the end of a timeout's cooldown and
/// whether a check is running, so a hung git never stalls a command twice within the cooldown
/// and never piles up processes. The clock is passed in (tests use their own).
pub(crate) struct GitCheckGate {
    cooldown: std::time::Duration,
    state: std::sync::Mutex<GateState>,
}

#[derive(Default)]
struct GateState {
    until: std::collections::HashMap<PathBuf, std::time::Instant>,
    running: BTreeSet<PathBuf>,
}

impl GitCheckGate {
    pub(crate) fn new(cooldown: std::time::Duration) -> GitCheckGate {
        GitCheckGate {
            cooldown,
            state: std::sync::Mutex::new(GateState::default()),
        }
    }

    /// Start a check of `repo` at `now`, or why not (a cooldown, a check still running).
    pub(crate) fn begin(&self, repo: &Path, now: std::time::Instant) -> Result<(), String> {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(until) = s.until.get(repo).copied() {
            if now < until {
                let left = (until - now).as_secs_f64().ceil() as u64;
                return Err(format!("git config check timed out; retry in {left}s"));
            }
            s.until.remove(repo);
        }
        if !s.running.insert(repo.to_path_buf()) {
            return Err("a git config check of this repository is still running".into());
        }
        Ok(())
    }

    /// The check of `repo` that began ended at `now`; a timeout starts the cooldown.
    pub(crate) fn end(&self, repo: &Path, now: std::time::Instant, timed_out: bool) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.running.remove(repo);
        if timed_out {
            s.until.insert(repo.to_path_buf(), now + self.cooldown);
        }
    }
}

fn git_gate() -> &'static GitCheckGate {
    static GATE: std::sync::OnceLock<GitCheckGate> = std::sync::OnceLock::new();
    GATE.get_or_init(|| GitCheckGate::new(GIT_CHECK_COOLDOWN))
}

/// The common dir of the git directory `dir` (its `commondir`, else itself), canonical when it
/// can be: the key of [`GitCheckGate`].
fn common_of(dir: &Path) -> PathBuf {
    read_trim(&dir.join("commondir"))
        .and_then(|c| canon(&resolve_rel(dir, &c)))
        .or_else(|| canon(dir))
        .unwrap_or_else(|| dir.to_path_buf())
}

/// [`run_git_config`] for a config file of the repository `repo`, through [`git_gate`].
fn git_include_values(p: &Path, repo: &Path) -> Option<Result<Vec<Vec<u8>>, String>> {
    let gate = git_gate();
    if let Err(why) = gate.begin(repo, std::time::Instant::now()) {
        return Some(Err(why));
    }
    let r = run_git_config(p);
    let timed_out = matches!(r, Some(Err((_, true))));
    gate.end(repo, std::time::Instant::now(), timed_out);
    r.map(|r| r.map_err(|(why, _)| why))
}

/// At most this much `git config` output is read; more is an error.
const GIT_CHECK_MAX_OUTPUT: u64 = 1 << 20;

/// [`run_git_config`]'s answer: the include values, or why not and whether it timed out.
type GitAnswer = Result<Vec<Vec<u8>>, (String, bool)>;

/// The include values git reads from the config file `p` alone, `None` when git cannot be
/// started; an error says whether it timed out. One deadline ([`GIT_CHECK_TIMEOUT`]) bounds
/// all of it: the child's exit and the end of its stdout (a grandchild may hold the pipe). On expiry the child's process group is
/// killed, the reader thread is left to end when the pipe closes (never joined), and the
/// result is an error.
fn run_git_config(p: &Path) -> Option<GitAnswer> {
    use std::io::Read as _;
    use std::os::unix::process::CommandExt as _;
    use std::process::{Command, Stdio};
    let mut cmd = Command::new("git");
    cmd.arg("config")
        .arg("--file")
        .arg(p)
        .args([
            "--no-includes",
            "--null",
            "--get-regexp",
            r"^(include|includeif\..*)\.path$",
        ])
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(k);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CEILING_DIRECTORIES", "/")
        .env("GIT_TERMINAL_PROMPT", "0");
    let deadline = std::time::Instant::now() + GIT_CHECK_TIMEOUT;
    let mut child = cmd.spawn().ok()?;
    let pgid = child.id() as libc::pid_t;
    let hung = |child: &mut std::process::Child| {
        crate::run::signal_group(pgid, libc::SIGKILL);
        let _ = child.kill();
        let _ = child.wait();
        Some(Err((
            format!(
                "git config check timed out; retry in {}s",
                GIT_CHECK_COOLDOWN.as_secs()
            ),
            true,
        )))
    };
    let Some(stdout) = child.stdout.take() else {
        return hung(&mut child);
    };
    let (tx, rx) = std::sync::mpsc::channel();
    // Detached: it ends when the pipe closes (at the latest when the group is killed).
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let ok = stdout
            .take(GIT_CHECK_MAX_OUTPUT + 1)
            .read_to_end(&mut buf)
            .is_ok();
        let _ = tx.send((ok, buf));
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            _ => return hung(&mut child),
        }
    };
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    let Ok((ok, out)) = rx.recv_timeout(left) else {
        return hung(&mut child);
    };
    // The child is gone; whatever else is left of its group goes too.
    crate::run::signal_group(pgid, libc::SIGKILL);
    if !ok || out.len() as u64 > GIT_CHECK_MAX_OUTPUT {
        return Some(Err(("the git config check output cannot be read".into(), false)));
    }
    match status.code() {
        Some(1) if out.is_empty() => Some(Ok(Vec::new())),
        Some(0) => Some(
            out.split(|b| *b == 0)
                .filter(|e| !e.is_empty())
                .map(|e| match e.iter().position(|b| *b == b'\n') {
                    Some(i) => Ok(e[i + 1..].to_vec()),
                    None => Err(("git lists an include without a value".to_string(), false)),
                })
                .collect(),
        ),
        _ => Some(Err(("git cannot read the config".into(), false))),
    }
}

/// sbx-gitsym: the mounts a command sees, for "could the command change this path".
pub(crate) struct Mounts {
    /// Read-write binds (the writable cwd, writable caches), canonical.
    pub rw: Vec<PathBuf>,
    /// Read-only binds inside them (the repository's git directories), canonical.
    pub ro: Vec<PathBuf>,
}

impl Mounts {
    /// The command can change `q` (a physical path): its deepest mount is read-write and `q`
    /// lies below it, or is that mount and is the final target (its content is writable; a
    /// mount point itself cannot be renamed). A read-only mount wins a tie.
    fn writable(&self, q: &Path, last: bool) -> bool {
        let deepest = |xs: &[PathBuf]| {
            xs.iter()
                .filter(|m| q.starts_with(m))
                .map(|m| m.components().count())
                .max()
        };
        match (deepest(&self.rw), deepest(&self.ro)) {
            (Some(r), o) if o.is_none_or(|o| r > o) => r < q.components().count() || last,
            _ => false,
        }
    }
}

#[derive(Debug)]
enum Hop {
    Root,
    Up,
    Name(std::ffi::OsString),
}

fn hops_of(p: &Path) -> Vec<Hop> {
    use std::path::Component;
    p.components()
        .filter_map(|c| match c {
            Component::RootDir | Component::Prefix(_) => Some(Hop::Root),
            Component::CurDir => None,
            Component::ParentDir => Some(Hop::Up),
            Component::Normal(n) => Some(Hop::Name(n.to_os_string())),
        })
        .collect()
}

/// sbx-gitsym: follow `start` (absolute) component by component as the kernel would, and refuse
/// when a directory on the way, a symlink of the chain or the final target is something the
/// command could change, or the path cannot be followed (dangling, a loop, too many hops).
/// Returns the physical final path.
fn walk_alias(start: &Path, m: &Mounts) -> Result<PathBuf, String> {
    let mut queue: std::collections::VecDeque<Hop> = hops_of(start).into();
    let mut cur = PathBuf::from("/");
    let mut links = 0usize;
    while let Some(h) = queue.pop_front() {
        match h {
            Hop::Root => cur = PathBuf::from("/"),
            Hop::Up => {
                cur.pop();
            }
            Hop::Name(n) => {
                let next = cur.join(&n);
                if m.writable(&next, queue.is_empty()) {
                    return Err(format!(
                        "{} reaches {}, which a command can change",
                        start.display(),
                        next.display()
                    ));
                }
                match fs::symlink_metadata(&next) {
                    Ok(md) if md.file_type().is_symlink() => {
                        links += 1;
                        if links > MAX_SYMLINK_HOPS {
                            return Err(format!("{} has too many symlinks", start.display()));
                        }
                        let target = fs::read_link(&next)
                            .map_err(|e| format!("{} cannot be followed ({e})", next.display()))?;
                        for hop in hops_of(&target).into_iter().rev() {
                            queue.push_front(hop);
                        }
                    }
                    Ok(_) => cur = next,
                    Err(e) => {
                        return Err(format!(
                            "{} cannot be followed at {} ({e})",
                            start.display(),
                            next.display()
                        ))
                    }
                }
            }
        }
    }
    Ok(cur)
}

/// sbx-gitsym: why commands must be refused in a repository whose git directories `dirs` are
/// bound read-only (`m.ro`), or `None` when every alias of their protected entries (`config`,
/// `config.worktree`, `hooks` and below, every `include`) ends outside what the command can
/// change, with no hard-linked file.
pub(crate) fn alias_refusal(dirs: &[PathBuf], m: &Mounts, home: &Path) -> Option<String> {
    for d in dirs {
        let repo = common_of(d);
        for name in ["config", "config.worktree"] {
            let p = d.join(name);
            if fs::symlink_metadata(&p).is_err_and(|e| e.kind() == ErrorKind::NotFound) {
                continue;
            }
            if let Err(why) = check_config(&p, &repo, m, home, 0) {
                return Some(why);
            }
        }
        let hooks = d.join("hooks");
        if fs::symlink_metadata(&hooks).is_err_and(|e| e.kind() == ErrorKind::NotFound) {
            continue;
        }
        if let Err(why) = walk_alias(&hooks, m).and_then(|t| check_tree(&hooks, &t, m)) {
            return Some(why);
        }
    }
    None
}

/// The config file `p` (and what it includes) is safe from the command.
fn check_config(p: &Path, repo: &Path, m: &Mounts, home: &Path, depth: usize) -> Result<(), String> {
    if depth > MAX_INCLUDE_DEPTH {
        return Err(format!("{} is included too deeply", p.display()));
    }
    let real = walk_alias(p, m)?;
    let md = fs::metadata(&real).map_err(|e| format!("{} cannot be checked ({e})", real.display()))?;
    if !md.is_file() {
        return Err(format!("{} is not a regular file", real.display()));
    }
    if md.nlink() > 1 {
        return Err(format!("{} has more than one hard link", real.display()));
    }
    let inc = config_includes(&real, repo)?;
    for raw in inc.paths {
        use std::os::unix::ffi::OsStrExt as _;
        // git resolves a relative include against the file it opened; check that directory
        // and the physical one. The value's bytes are the file name, exactly.
        let mut targets: Vec<PathBuf> = Vec::new();
        if let Some(rest) = raw.strip_prefix(b"~/") {
            targets.push(home.join(std::ffi::OsStr::from_bytes(rest)));
        } else if raw.starts_with(b"~") || raw.starts_with(b"%(") {
            return Err(format!(
                "{}: include path {:?} is not checked",
                p.display(),
                String::from_utf8_lossy(&raw)
            ));
        } else if raw.starts_with(b"/") {
            targets.push(PathBuf::from(std::ffi::OsStr::from_bytes(&raw)));
        } else {
            let raw = std::ffi::OsStr::from_bytes(&raw);
            for base in [p.parent(), real.parent()].into_iter().flatten() {
                targets.push(base.join(raw));
            }
            targets.dedup();
        }
        for t in targets {
            check_config(&t, repo, m, home, depth + 1)?;
        }
    }
    Ok(())
}

/// Everything below the hooks directory `real` (reached from `hooks`) is safe from the command:
/// every entry outside what it can change, symlinks followed, files with one link.
fn check_tree(hooks: &Path, real: &Path, m: &Mounts) -> Result<(), String> {
    if !fs::metadata(real).is_ok_and(|md| md.is_dir()) {
        return Err(format!("{} is not a directory", hooks.display()));
    }
    let mut stack = vec![real.to_path_buf()];
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        let entries = fs::read_dir(&d).map_err(|e| format!("{} cannot be read ({e})", d.display()))?;
        for entry in entries {
            let p = entry
                .map_err(|e| format!("{} cannot be read ({e})", d.display()))?
                .path();
            seen += 1;
            if seen > MAX_HOOKS_ENTRIES {
                return Err(format!("{} has too many entries to check", hooks.display()));
            }
            let t = walk_alias(&p, m)?;
            let md = fs::metadata(&t).map_err(|e| format!("{} cannot be checked ({e})", t.display()))?;
            if md.is_dir() {
                stack.push(t);
            } else if !md.is_file() || md.nlink() > 1 {
                return Err(format!("{} is not a regular file with one link", t.display()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod include_tests {
    use super::*;

    fn paths(text: &str) -> Vec<String> {
        parse_includes(text.as_bytes())
            .expect("parses")
            .paths
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect()
    }

    #[test]
    fn values_and_comments_are_not_include_headers() {
        for text in [
            "[remote \"origin\"]\n\turl = https://example.invalid/include-lib.git\n",
            "#include something\n; [include]\n\t# [include]\n",
            "[core]\n\teditor = vim # [include]\n",
            "[remote \"[include]\"]\n\turl = x\n",
            "[alias]\n\tx = !echo \\\n[include]\n",
            "[includes]\n\tpath = x\n[included \"a\"]\n\tpath = y\n",
        ] {
            assert_eq!(
                parse_includes(text.as_bytes()),
                Ok(Includes::default()),
                "{text:?}"
            );
        }
    }

    #[test]
    fn values_keep_git_bytes() {
        let cases: &[(&str, &str)] = &[
            ("[include]\n\tpath = \" target\"\n", " target"),
            ("[include]\n\tpath = \"target \"\n", "target "),
            ("[include]\n\tpath = \"\\ttarget\"\n", "\ttarget"),
            ("[include]\n\tpath = \"\ttarget\"\n", "\ttarget"),
            ("[include]\n\tpath = tar\\\"get\n", "tar\"get"),
            ("[include]\n\tpath = tar\\\nget\n", "target"),
            ("[include]\n\tpath = \" tar\\\nget \"\n", " target "),
            ("[include]\n\tpath =   a  b   ; c\n", "a  b"),
            ("[include]\n\tpath = a\" b \"  \n", "a b "),
            ("[include]\r\n\tpath = a\\\\b\r\n", "a\\b"),
            ("[Include] PATH=x", "x"),
            ("[includeIf \"gitdir:/x/\"]\n\tpath = y\n", "y"),
            ("[includeIf \"onbranch:]\\\"\"]\n\tpath=../x\n", "../x"),
            ("[includeif.cond]\n\tpath = z\n", "z"),
        ];
        for (text, want) in cases {
            assert_eq!(paths(text), vec![(*want).to_string()], "{text:?}");
        }
        // Not includes for git: a subsection of `include`, a legacy `include.sub`.
        for text in ["[include \"x\"]\n\tpath = a\n", "[include.sub]\n\tpath = a\n"] {
            let got = parse_includes(text.as_bytes()).expect("parses");
            assert!(got.header && got.paths.is_empty(), "{text:?}: {got:?}");
        }
    }

    #[test]
    fn syntax_git_rejects_or_that_is_unsure_is_an_error() {
        for text in [
            "[include\n",
            "[include]\n\tpath\n",
            "[include]\n\tpath = \"a\n",
            "[include]\n\tpath = a\\qb\n",
            "[include]\n\tpath = a\0b\n",
            "[include]\n\tpath = \n",
            "[ include ]\n",
            "[include \"x]\n",
            "[core]\n\t1x = y\n",
        ] {
            assert!(parse_includes(text.as_bytes()).is_err(), "{text:?}");
        }
    }

    /// The parser and git agree on a corpus of tricky values (skips without git).
    #[test]
    fn parser_matches_git_on_tricky_values() {
        let dir = std::env::temp_dir().join(format!("oxa-gitsym-parse-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("mkdir");
        let corpus = [
            "[include]\n\tpath = \" lead\"\n\tpath = \"trail \"\n\tpath = \"\\ttab\"\n",
            "[include]\n\tpath = \"\tlit\"\n\tpath = esc\\\"q\n\tpath = co\\\nnt\n",
            "[include]\n\tpath =   in  ner   ; c\n\tpath = a\" b \"  # x\n",
            "[includeIf \"gitdir:/x/\"] path = y\n[include \"s\"]\n\tpath = no\n[include.sub]\n\tpath = no\n",
            "[remote \"o\"]\n\turl = https://example.invalid/include-lib.git\n#include\n",
        ];
        for (i, text) in corpus.iter().enumerate() {
            let f = dir.join(format!("c{i}"));
            fs::write(&f, text).expect("write");
            let Some(git) = run_git_config(&f) else {
                return;
            };
            let git = git.map_err(|(why, _)| why);
            assert_eq!(
                git,
                Ok(parse_includes(text.as_bytes()).expect("parses").paths),
                "{text:?}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_timeout_cools_down_only_its_own_repository_then_retries() {
        use std::time::{Duration, Instant};
        let gate = GitCheckGate::new(Duration::from_secs(60));
        let (a, b) = (Path::new("/repo-a/.git"), Path::new("/repo-b/.git"));
        let t0 = Instant::now();
        gate.begin(a, t0).expect("first check");
        gate.end(a, t0, true);
        // The same repository within the cooldown: refused at once, the message names it.
        let err = gate
            .begin(a, t0 + Duration::from_secs(1))
            .expect_err("cooling down");
        assert_eq!(err, "git config check timed out; retry in 59s");
        assert!(gate.begin(a, t0 + Duration::from_millis(59_500)).is_err());
        // Another repository: checked normally.
        gate.begin(b, t0 + Duration::from_secs(1)).expect("other repo");
        gate.end(b, t0 + Duration::from_secs(1), false);
        gate.begin(b, t0 + Duration::from_secs(2))
            .expect("other repo again");
        gate.end(b, t0 + Duration::from_secs(2), false);
        // After the cooldown: one fresh attempt.
        gate.begin(a, t0 + Duration::from_secs(61)).expect("retried");
        gate.end(a, t0 + Duration::from_secs(61), false);
        gate.begin(a, t0 + Duration::from_secs(62))
            .expect("and normal again");
    }

    #[test]
    fn at_most_one_check_per_repository_runs() {
        use std::time::{Duration, Instant};
        let gate = GitCheckGate::new(Duration::from_secs(60));
        let a = Path::new("/repo-a/.git");
        let t0 = Instant::now();
        gate.begin(a, t0).expect("first");
        let err = gate.begin(a, t0).expect_err("second while running");
        assert!(err.contains("still running"), "{err}");
        gate.begin(Path::new("/repo-b/.git"), t0)
            .expect("other repo runs");
        gate.end(a, t0, false);
        gate.begin(a, t0).expect("after it ended");
    }
}
