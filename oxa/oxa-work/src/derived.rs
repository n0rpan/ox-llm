//! Done checks derived from what a task actually changed (#1442): the tests and clippy run
//! of every Cargo crate the worker touched, next to the project's static list, plus crates
//! the worker declared as the task's acceptance tests.
//!
//! Working-tree inventory uses the existing sandbox because Git can run clean filters.
//! Committed inventory compares immutable objects. Every derived entry is a fixed shell
//! shape over validated crate names, so worker text can never become a check command. A
//! check that could not run (toolchain missing, timeout) is `not_run`, and a task with any
//! such check ends `unverified`, never `done`.
use crate::{git, output, RunPolicy};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// One check the runner ran, with the evidence its verdict rests on (#1442). The command is
/// the entry as configured or derived, bounded by [`COMMAND_CAP`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CheckRun {
    pub origin: CheckOrigin,
    pub command: String,
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub status: CheckStatus,
}

/// Where a check came from: the project's frozen static list, the change's crates, or a
/// crate the worker declared as acceptance.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckOrigin {
    Settings,
    Derived,
    Declared,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    NotRun,
}

/// Check entries of one acceptance round, plus the crates whose tests they run.
#[derive(Default)]
pub(crate) struct DerivedChecks {
    /// Full `in <dir>: <command>` entries, sorted for stable evidence.
    pub commands: Vec<String>,
    /// Crate names covered by a test command (declared duplicates are skipped).
    pub tested: BTreeSet<String>,
}

/// A worker's declared acceptance crates, as read from its final message. An unreadable
/// block keeps the task unverifiable instead of silently dropping the claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Declaration {
    Absent,
    Tests(Vec<String>),
    Unreadable,
}

/// Selector stored for a block that was present but not readable crate names.
pub(crate) const UNREADABLE_DECLARATION: &str = "__unreadable__";

/// Total checks of one acceptance round. Beyond this the round cannot honestly run, and
/// the task ends `unverified` with the reason instead of a partial verdict.
pub(crate) const MAX_CHECKS: usize = 256;
const MAX_PACKAGES: usize = 64;
const MAX_CHANGED_PATHS: usize = 4096;
const MAX_DECLARED: usize = 16;
const MAX_MEMBERS: usize = 256;
const MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_MANIFESTS: usize = 512;
const MAX_WALK_ENTRIES: usize = 20_000;
/// Bytes of one check command kept in evidence.
pub(crate) const COMMAND_CAP: usize = 512;

/// Exit statuses meaning the check never executed: the executable, component or toolchain
/// was missing. Any other status is a real verdict about the change.
pub(crate) const NOT_RUN_EXIT: &[i32] = &[126, 127];

/// The crates of one change, as `cargo test`/`cargo clippy` entries. `Err` is a reason the
/// derived set could not be planned; the task then ends `unverified`, never `done`.
pub(crate) fn derive(
    policy: &RunPolicy,
    root: &Path,
    base: &str,
    head: &str,
) -> Result<DerivedChecks, String> {
    let diff = output(git(
        policy,
        root,
        &[
            "diff",
            "--name-only",
            "--no-relative",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=none",
            "-z",
            base,
            head,
            "--",
        ],
    ))?;
    if !diff.status.success() {
        return Err("committed change inventory failed".into());
    }
    from_changed_paths(root, &diff.stdout)
}

pub(crate) fn from_changed_paths(root: &Path, paths: &[u8]) -> Result<DerivedChecks, String> {
    let paths = std::str::from_utf8(paths).map_err(|_| "change inventory is not UTF-8")?;
    let paths = if paths.is_empty() {
        Vec::new()
    } else {
        let paths = paths.strip_suffix('\0').ok_or("incomplete change inventory")?;
        let paths: BTreeSet<_> = paths.split('\0').collect();
        if paths.len() > MAX_CHANGED_PATHS {
            return Err(format!("more than {MAX_CHANGED_PATHS} changed paths"));
        }
        if paths.iter().any(|path| {
            path.split('/')
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
        }) {
            return Err("invalid path in change inventory".into());
        }
        paths.into_iter().map(str::to_owned).collect()
    };
    Ok(commands(&select(root, &paths)?))
}

/// One parsed `Cargo.toml`: the package name, whether it roots a workspace, and that
/// workspace's member paths (globs allowed; comments and multiline tolerated).
#[derive(Default, Debug, PartialEq, Eq)]
struct Manifest {
    package: Option<String>,
    workspace: bool,
    members: Vec<String>,
}

/// Read a bounded regular manifest beneath the checkout. Walk by descriptor so
/// project-controlled symlinks cannot redirect host-side reads.
fn read_manifest(root: &Path, dir: &str) -> Result<Option<Manifest>, String> {
    fn open_child(parent: &File, name: &str, flags: libc::c_int) -> io::Result<File> {
        let name = CString::new(name)?;
        // SAFETY: the parent descriptor and C string remain valid across openat.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a new descriptor owned by this function.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    let read = || -> io::Result<Vec<u8>> {
        let mut folder = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(root)?;
        if !dir.is_empty() {
            if !valid_member_path(dir) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid checkout path",
                ));
            }
            for part in dir.split('/') {
                folder = open_child(&folder, part, libc::O_RDONLY | libc::O_DIRECTORY)?;
            }
        }
        let file = open_child(&folder, "Cargo.toml", libc::O_RDONLY | libc::O_NONBLOCK)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MANIFEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected a bounded regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MANIFEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "manifest exceeds the read limit",
            ));
        }
        Ok(bytes)
    };

    match read() {
        Ok(bytes) => std::str::from_utf8(&bytes)
            .map(|text| Some(parse_manifest(text)))
            .map_err(|_| format!("{} is not UTF-8", join_dir(dir, "Cargo.toml"))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", join_dir(dir, "Cargo.toml"))),
    }
}

/// A line-oriented subset of TOML: section headers, `[package] name`, a `[workspace]` flag
/// and `members` lists. It never fails; an unknown shape reads as an empty manifest.
fn parse_manifest(text: &str) -> Manifest {
    let mut out = Manifest::default();
    let mut section = String::new();
    let mut members = None::<String>;
    for line in text.lines() {
        let line = manifest_line(line);
        if let Some(buffer) = &mut members {
            buffer.push(' ');
            buffer.push_str(line);
            if line.contains(']') {
                out.members = quoted_items(buffer);
                members = None;
            }
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('[') {
            section = rest.split(']').next().unwrap_or(rest).to_owned();
            if section == "workspace" || section.starts_with("workspace.") {
                out.workspace = true;
            }
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            let (key, value) = (key.trim(), value.trim());
            if section == "package" && out.package.is_none() && key == "name" {
                out.package = unquote(value).filter(|name| valid_package_name(name));
            }
            if section == "workspace" && key == "members" {
                if value.contains(']') {
                    out.members = quoted_items(value);
                } else {
                    members = Some(value.to_owned());
                }
            }
        }
    }
    out.members.truncate(MAX_MEMBERS);
    out
}

/// Remove a TOML comment before folding a line into a multiline members list.
fn manifest_line(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in line.char_indices() {
        if escaped {
            escaped = false;
        } else if quote == Some('"') && ch == '\\' {
            escaped = true;
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() {
            match ch {
                '#' => return &line[..index],
                '"' | '\'' => quote = Some(ch),
                _ => {}
            }
        }
    }
    line
}

/// The quoted strings of a `members` value, without comments, in order.
fn quoted_items(value: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find(['"', '\'']) {
        let quote = rest[open..].chars().next().expect("non-empty");
        let Some(end) = rest[open + 1..].find(quote) else {
            break;
        };
        let item = &rest[open + 1..open + 1 + end];
        if valid_member_path(item) {
            items.push(item.to_owned());
        }
        rest = &rest[open + 1 + end + 1..];
    }
    items
}

/// Member paths are relative workspace locations: no `..`, absolute paths or empty parts.
fn valid_member_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 256
        && !path.starts_with('/')
        && !path.contains(':')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != ".." && part != "." && part.trim() == part)
}

fn unquote(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let quote = *bytes.first()?;
    if (quote == b'"' || quote == b'\'') && bytes.last() == Some(&quote) && bytes.len() >= 2 {
        return Some(value[1..value.len() - 1].to_owned());
    }
    None
}

fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

/// `p` without its final component; "" at the tree root.
fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(at) => &path[..at],
        None => "",
    }
}

fn join_dir(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// Manifests read so far, keyed by relative directory ("" is the tree root).
/// Only absent manifests select nothing.
#[derive(Default)]
struct Manifests {
    root: BTreeMap<String, Manifest>,
}

impl Manifests {
    fn at(&mut self, root: &Path, dir: &str) -> Result<&Manifest, String> {
        if !self.root.contains_key(dir) {
            let manifest = read_manifest(root, dir)?.unwrap_or_default();
            self.root.insert(dir.to_owned(), manifest);
        }
        Ok(&self.root[dir])
    }
    /// The directories at and above `dir` with a package or workspace manifest, deepest
    /// first, reading manifests on the way.
    fn ancestors(&mut self, root: &Path, dir: &str) -> Result<Vec<String>, String> {
        let mut chain = vec![dir.to_owned()];
        let mut current = dir;
        while let Some(at) = current.rfind('/') {
            current = &current[..at];
            chain.push(current.to_owned());
        }
        if dir.is_empty() {
            chain.clear();
        }
        chain.push(String::new());
        let mut found = Vec::new();
        for dir in chain {
            let manifest = self.at(root, &dir)?;
            if manifest.package.is_some() || manifest.workspace {
                found.push(dir);
            }
        }
        Ok(found)
    }
    /// The directory a cargo command for the crate at `dir` runs in: the nearest workspace
    /// root at or above it, or the crate directory itself for a standalone crate.
    fn in_dir(&mut self, root: &Path, dir: &str) -> Result<String, String> {
        for ancestor in self.ancestors(root, dir)? {
            if self.root[&ancestor].workspace {
                return Ok(ancestor);
            }
        }
        Ok(dir.to_owned())
    }
}

/// The crates of the changed paths. A workspace's own manifest or lockfile selects every
/// member (like the CI rule that unknown paths conservatively select every component);
/// other files outside a crate select nothing.
fn select(root: &Path, paths: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut manifests = Manifests::default();
    let mut selected: BTreeMap<String, String> = BTreeMap::new();
    for path in paths {
        let name = path.rsplit('/').next().unwrap_or(path.as_str());
        let dir = parent_dir(path);
        if name == "Cargo.toml" || name == "Cargo.lock" {
            let manifest = manifests.at(root, dir)?;
            if manifest.workspace {
                let package = manifest.package.clone();
                let members = manifest.members.clone();
                for member in members {
                    for member_dir in member_dirs(root, dir, &member) {
                        if let Some(package) = manifests.at(root, &member_dir)?.package.clone() {
                            selected.insert(package, dir.to_owned());
                        }
                    }
                }
                if let Some(package) = package {
                    selected.insert(package, dir.to_owned());
                }
            } else if let Some(package) = manifest.package.clone() {
                selected.insert(package, manifests.in_dir(root, dir)?);
            }
            continue;
        }
        for ancestor in manifests.ancestors(root, dir)? {
            if let Some(package) = manifests.root[&ancestor].package.clone() {
                let in_dir = manifests.in_dir(root, &ancestor)?;
                selected.insert(package, in_dir);
                break;
            }
        }
    }
    if selected.len() > MAX_PACKAGES {
        return Err(format!(
            "the change affects more than {MAX_PACKAGES} crates, so their checks cannot be listed"
        ));
    }
    Ok(selected)
}

/// The directories a `members` entry names, with `*`/`?` glob components expanded.
fn member_dirs(root: &Path, base: &str, member: &str) -> Vec<String> {
    let full = join_dir(base, member);
    if !full.contains(['*', '?']) {
        return valid_member_path(&full).then_some(full).into_iter().collect();
    }
    let mut frontier = vec![String::new()];
    for part in full.split('/') {
        let mut next = Vec::new();
        for dir in &frontier {
            if part.contains('*') || part.contains('?') {
                next.extend(glob_dirs(root, dir, part));
            } else if valid_member_path(&join_dir(dir, part)) {
                next.push(join_dir(dir, part));
            }
        }
        frontier = next;
        if frontier.is_empty() {
            return vec![];
        }
    }
    frontier.retain(|dir| valid_member_path(dir));
    frontier.sort();
    frontier.truncate(MAX_MEMBERS);
    frontier
}

/// The subdirectories of `dir` matching `pattern`.
fn glob_dirs(root: &Path, dir: &str, pattern: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
        return vec![];
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(file) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if simple_glob(pattern, &file) {
            out.push(join_dir(dir, &file));
        }
    }
    out.sort();
    out
}

/// `*` and `?` only, anchored at both ends; member globs never need character classes.
fn simple_glob(pattern: &str, name: &str) -> bool {
    fn matches(pattern: &[u8], name: &[u8]) -> bool {
        match (pattern.first(), name.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some(b'*'), _) => {
                matches(&pattern[1..], name) || (!name.is_empty() && matches(pattern, &name[1..]))
            }
            (Some(b'?'), Some(_)) => matches(&pattern[1..], &name[1..]),
            (Some(want), Some(have)) if want == have => matches(&pattern[1..], &name[1..]),
            _ => false,
        }
    }
    matches(pattern.as_bytes(), name.as_bytes())
}

/// The check entries of the selected crates: `cargo test` plus clippy, one per crate.
fn commands(selected: &BTreeMap<String, String>) -> DerivedChecks {
    let mut out = DerivedChecks::default();
    for (name, in_dir) in selected {
        out.tested.insert(name.clone());
        out.commands.push(test_entry(in_dir, name));
        out.commands.push(clippy_entry(in_dir, name));
    }
    out
}

/// The directory part of a derived entry: the tree root writes itself as `.` so the entry
/// never reads `in : <command>`.
fn entry_dir(in_dir: &str) -> &str {
    if in_dir.is_empty() {
        "."
    } else {
        in_dir
    }
}

/// The tests of one crate. `--frozen` keeps the lockfile and the network out of the
/// worktree; a cold registry or a drifted lockfile is a failure to fix, not a silent pass.
fn test_entry(in_dir: &str, name: &str) -> String {
    format!(
        "in {}: command -v cargo >/dev/null 2>&1 || exit 127\ncargo test --frozen -p {name}",
        entry_dir(in_dir)
    )
}

/// Clippy of one crate, guarded so a missing component is `not_run` (the task stays
/// `unverified`) instead of cargo's generic failure.
fn clippy_entry(in_dir: &str, name: &str) -> String {
    format!(
        "in {}: cargo clippy --version >/dev/null 2>&1 || exit 127\ncargo clippy --frozen -p {name} -- -D warnings",
        entry_dir(in_dir)
    )
}

/// Every crate of the tree, bounded. `None`: the walk exceeded its capacity, so declared
/// selectors cannot be resolved and the task stays `unverified` rather than half-checked.
pub(crate) fn discover(root: &Path) -> Option<BTreeMap<String, String>> {
    let mut manifests = Manifests::default();
    let mut stack = vec![String::new()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let path = if dir.is_empty() {
            root.to_owned()
        } else {
            root.join(&dir)
        };
        let entries = std::fs::read_dir(path).ok()?;
        for entry in entries {
            seen += 1;
            if seen > MAX_WALK_ENTRIES || manifests.root.len() > MAX_MANIFESTS {
                return None;
            }
            let entry = entry.ok()?;
            let file_type = entry.file_type().ok()?;
            let Some(file) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let full = join_dir(&dir, &file);
            if file_type.is_dir() {
                if !matches!(file.as_str(), ".git" | "target" | "node_modules") {
                    stack.push(full);
                }
            } else if file == "Cargo.toml" {
                let manifest_dir = full.strip_suffix("/Cargo.toml").unwrap_or("").to_owned();
                let manifest = read_manifest(root, &manifest_dir).ok()??;
                manifests.root.insert(manifest_dir, manifest);
            }
        }
    }
    let mut packages = BTreeMap::new();
    let dirs: Vec<String> = manifests.root.keys().cloned().collect();
    for dir in dirs {
        if let Some(name) = manifests.root[&dir].package.clone() {
            let in_dir = manifests.in_dir(root, &dir).ok()?;
            packages.insert(name, in_dir);
        }
    }
    Some(packages)
}

/// Map the worker's declared acceptance crates to test entries. `Err` lists the selectors
/// that cannot run; the caller keeps the task `unverified` instead of skipping them.
pub(crate) fn declared_entries(
    declared: &[String],
    discovered: &BTreeMap<String, String>,
    already_tested: &BTreeSet<String>,
) -> Result<Vec<String>, Vec<String>> {
    let mut commands = Vec::new();
    let mut missing = Vec::new();
    for selector in declared {
        if selector == UNREADABLE_DECLARATION {
            missing.push("the declared acceptance block was not readable crate names".to_owned());
        } else if already_tested.contains(selector) {
            continue;
        } else if let Some(in_dir) = discovered.get(selector) {
            commands.push(test_entry(in_dir, selector));
        } else {
            missing.push(format!(
                "the declared acceptance crate {selector} is not a crate of this repository"
            ));
        }
    }
    if missing.is_empty() {
        Ok(commands)
    } else {
        Err(missing)
    }
}

/// The worker's `<oxa-acceptance>` block: a JSON array of crate names. Present but
/// unreadable content is [`Declaration::Unreadable`] so it can never be silently dropped.
pub(crate) fn parse_declaration(output: &str) -> Declaration {
    let Some((_, rest)) = output.rsplit_once("<oxa-acceptance>") else {
        return Declaration::Absent;
    };
    let Some((json, _)) = rest.split_once("</oxa-acceptance>") else {
        return Declaration::Unreadable;
    };
    if json.len() > 4096 {
        return Declaration::Unreadable;
    }
    let Ok(selectors) = serde_json::from_str::<Vec<String>>(json) else {
        return Declaration::Unreadable;
    };
    if selectors.len() > MAX_DECLARED || selectors.iter().any(|s| !valid_package_name(s)) {
        return Declaration::Unreadable;
    }
    Declaration::Tests(selectors)
}

/// Merge one worker run's declaration into the execution's durable set: what was claimed
/// must be verified even if a later repair round drops the block. A set that cannot be
/// honored whole carries the unreadable marker, so the task stays `unverified` instead of
/// quietly dropping the surplus.
pub(crate) fn merge_declaration(declared: Option<Vec<String>>, next: &Declaration) -> Option<Vec<String>> {
    let mut merged = declared.unwrap_or_default();
    match next {
        Declaration::Absent => {}
        Declaration::Tests(selectors) => merged.extend(selectors.iter().cloned()),
        Declaration::Unreadable => merged.push(UNREADABLE_DECLARATION.to_owned()),
    }
    let mut seen = BTreeSet::new();
    merged.retain(|selector| seen.insert(selector.clone()));
    if merged.len() > MAX_DECLARED {
        merged.truncate(MAX_DECLARED - 1);
        merged.push(UNREADABLE_DECLARATION.to_owned());
    }
    (!merged.is_empty()).then_some(merged)
}

/// What one check's exit means: a clean run, a real verdict about the change, or a check
/// that never executed (timeout, kill, missing executable or component). The last keeps the
/// task `unverified` rather than counting it as passed or failed.
pub(crate) fn classify(exit: Option<i32>, timed_out: bool) -> CheckStatus {
    if timed_out {
        return CheckStatus::NotRun;
    }
    match exit {
        None => CheckStatus::NotRun,
        Some(0) => CheckStatus::Passed,
        Some(code) if NOT_RUN_EXIT.contains(&code) => CheckStatus::NotRun,
        Some(_) => CheckStatus::Failed,
    }
}

/// The per-check evidence a check supervisor reports on its stdout: one `exit`/`timed_out`
/// fact per sent command, classified here. `None`: no complete, matching record, which
/// keeps the outcome `unverified` rather than trusting a bare exit. The supervisor is a
/// fact reporter; every verdict is decided by this side of the boundary.
pub(crate) fn parse_evidence(stdout: &str, expected: usize) -> Option<Vec<CheckRun>> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    if value["type"] != "oxa_checks" || value["version"] != 1 {
        return None;
    }
    let checks = value["checks"].as_array()?;
    if expected == 0 || checks.len() != expected {
        return None;
    }
    let mut runs = Vec::with_capacity(checks.len());
    for check in checks {
        let exit = match &check["exit"] {
            serde_json::Value::Null => None,
            value => {
                let code = value.as_i64()?;
                Some(i32::try_from(code).ok()?)
            }
        };
        let timed_out = check["timed_out"].as_bool()?;
        runs.push(CheckRun {
            origin: CheckOrigin::Settings,
            command: bounded_command(check["command"].as_str()?),
            status: classify(exit, timed_out),
            exit,
            timed_out,
        });
    }
    Some(runs)
}

/// A command as kept in evidence, with an explicit marker when it was cut.
pub(crate) fn bounded_command(command: &str) -> String {
    if command.len() <= COMMAND_CAP {
        return command.to_owned();
    }
    let mut end = COMMAND_CAP;
    while !command.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &command[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::PrivateDir;

    fn manifest(dir: &PrivateDir, path: &str, text: &str) {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().expect("manifest parent")).expect("create manifest folder");
        std::fs::write(full, text).expect("write manifest");
    }

    fn workspace() -> &'static str {
        r#"[workspace]
members = ["crates/*", "tools/pinned"]
"#
    }

    fn root_package_workspace() -> &'static str {
        r#"[package]
name = "top"
[workspace]
members = ["crates/*", "tools/pinned"]
"#
    }

    #[test]
    fn manifest_reading_keeps_members_globs_comments_and_names() {
        let parsed = parse_manifest(
            "[package]\nname = \"a-b_c\"\n[workspace]\nmembers = [\"crates/*\", # pinned below\n  \"tools/pinned\"]\n",
        );
        assert_eq!(parsed.package.as_deref(), Some("a-b_c"));
        assert!(parsed.workspace);
        assert_eq!(parsed.members, ["crates/*", "tools/pinned"]);

        // An unknown shape reads as an empty manifest, never as a failure.
        let empty = parse_manifest("] nonsense [garbage\nmembers = [\nnever closed");
        assert_eq!(empty, Manifest::default());

        // Invalid package names are ignored, not selected.
        let broken = parse_manifest("[package]\nname = \"not a name\"\n");
        assert_eq!(broken.package, None);
    }

    #[test]
    fn a_workspace_members_list_split_over_lines_is_read_whole() {
        let parsed = parse_manifest(
            "[workspace]\nmembers = [\n  \"one\",\n  \"two\",\n]\n[package]\nname = \"root\"\n",
        );
        assert_eq!(parsed.members, ["one", "two"]);
    }

    #[test]
    fn a_source_file_selects_its_crate_with_the_workspace_as_in_dir() {
        let dir = PrivateDir::new("derived-select");
        manifest(&dir, "Cargo.toml", workspace());
        manifest(&dir, "crates/one/Cargo.toml", "[package]\nname = \"one\"\n");
        manifest(&dir, "crates/two/Cargo.toml", "[package]\nname = \"two\"\n");
        let selected = select(dir.path(), &["crates/two/src/lib.rs".to_owned()]).unwrap();
        assert_eq!(selected, BTreeMap::from([("two".to_owned(), "".to_owned())]));
    }

    #[test]
    fn a_standalone_crate_selects_itself_as_its_own_in_dir() {
        let dir = PrivateDir::new("derived-standalone");
        manifest(&dir, "solo/Cargo.toml", "[package]\nname = \"solo\"\n");
        let selected = select(dir.path(), &["solo/src/main.rs".to_owned()]).unwrap();
        assert_eq!(selected, BTreeMap::from([("solo".to_owned(), "solo".to_owned())]));
    }

    #[test]
    fn a_workspace_manifest_or_lockfile_change_selects_every_member() {
        let dir = PrivateDir::new("derived-members");
        manifest(&dir, "Cargo.toml", root_package_workspace());
        manifest(&dir, "crates/one/Cargo.toml", "[package]\nname = \"one\"\n");
        manifest(&dir, "crates/two/Cargo.toml", "[package]\nname = \"two\"\n");
        manifest(&dir, "tools/pinned/Cargo.toml", "[package]\nname = \"pinned\"\n");
        for changed in ["Cargo.toml", "Cargo.lock"] {
            let selected = select(dir.path(), &[changed.to_owned()]).unwrap();
            assert_eq!(
                selected.keys().collect::<Vec<_>>(),
                ["one", "pinned", "top", "two"],
                "workspace-wide change {changed}"
            );
        }
    }

    #[test]
    fn a_standalone_lockfile_change_selects_its_package() {
        let dir = PrivateDir::new("derived-standalone-lock");
        manifest(&dir, "solo/Cargo.toml", "[package]\nname = \"solo\"\n");
        let selected = select(dir.path(), &["solo/Cargo.lock".to_owned()]).unwrap();
        assert_eq!(selected, BTreeMap::from([("solo".to_owned(), "solo".to_owned())]));
    }

    #[test]
    fn an_oversized_manifest_cannot_supply_crate_evidence() {
        let dir = PrivateDir::new("derived-bounded-manifest");
        let mut text = "[package]\nname = \"one\"\n#".to_owned();
        text.push_str(&"x".repeat(MANIFEST_BYTES as usize));
        manifest(&dir, "Cargo.toml", &text);
        let err = select(dir.path(), &["src/lib.rs".to_owned()]).unwrap_err();
        assert!(err.contains("bounded regular file"), "{err}");
        assert!(discover(dir.path()).is_none());
    }

    #[test]
    fn files_outside_any_crate_select_nothing() {
        let dir = PrivateDir::new("derived-none");
        manifest(&dir, "Cargo.toml", workspace());
        manifest(&dir, "crates/one/Cargo.toml", "[package]\nname = \"one\"\n");
        let selected = select(
            dir.path(),
            &["docs/page.md".to_owned(), "unrelated/Cargo.toml".to_owned()],
        )
        .unwrap();
        assert!(selected.is_empty());
    }

    #[test]
    fn a_file_of_a_root_package_selects_that_root_crate() {
        let dir = PrivateDir::new("derived-root-package");
        manifest(&dir, "Cargo.toml", root_package_workspace());
        let selected = select(dir.path(), &["docs/page.md".to_owned()]).unwrap();
        assert_eq!(selected, BTreeMap::from([("top".to_owned(), "".to_owned())]));
    }

    #[test]
    fn derived_entries_are_a_fixed_cargo_shape_per_crate() {
        let checks = commands(&BTreeMap::from([
            ("one".to_owned(), "".to_owned()),
            ("solo".to_owned(), "solo".to_owned()),
        ]));
        assert_eq!(
            checks.commands,
            [
                "in .: command -v cargo >/dev/null 2>&1 || exit 127\ncargo test --frozen -p one".to_owned(),
                "in .: cargo clippy --version >/dev/null 2>&1 || exit 127\ncargo clippy --frozen -p one -- -D warnings".to_owned(),
                "in solo: command -v cargo >/dev/null 2>&1 || exit 127\ncargo test --frozen -p solo".to_owned(),
                "in solo: cargo clippy --version >/dev/null 2>&1 || exit 127\ncargo clippy --frozen -p solo -- -D warnings".to_owned(),
            ]
        );
        assert_eq!(
            checks.tested,
            ["one".to_owned(), "solo".to_owned()]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn a_declaration_block_is_read_only_as_crate_names() {
        assert_eq!(parse_declaration("done without a block"), Declaration::Absent);
        assert_eq!(
            parse_declaration("done <oxa-acceptance>[\"one\",\"two\"]</oxa-acceptance>"),
            Declaration::Tests(["one".to_owned(), "two".to_owned()].to_vec())
        );
        // The last block wins when a reply carries two.
        assert_eq!(
            parse_declaration(
                "<oxa-acceptance>[\"old\"]</oxa-acceptance> <oxa-acceptance>[\"new\"]</oxa-acceptance>"
            ),
            Declaration::Tests(["new".to_owned()].to_vec())
        );
        for bad in [
            "<oxa-acceptance>[not json]</oxa-acceptance>",
            "<oxa-acceptance>[\"one\"]",
            "<oxa-acceptance>{\"one\":true}</oxa-acceptance>",
            "<oxa-acceptance>[\"bad name\"]</oxa-acceptance>",
            &format!("<oxa-acceptance>[{}]</oxa-acceptance>", "\"x\",".repeat(20)),
        ] {
            assert_eq!(parse_declaration(bad), Declaration::Unreadable, "{bad}");
        }
    }

    #[test]
    fn declarations_accumulate_across_rounds_and_cannot_overflow_quietly() {
        let mut declared = merge_declaration(None, &Declaration::Tests(vec!["one".into()]));
        declared = merge_declaration(declared, &Declaration::Absent);
        assert_eq!(
            declared.as_deref(),
            Some(["one".to_owned()].as_slice()),
            "a later round without a block keeps the earlier claim"
        );
        // A repeated claim is one claim.
        let merged = merge_declaration(declared, &Declaration::Tests(vec!["one".into()]));
        assert_eq!(merged.as_deref(), Some(["one".to_owned()].as_slice()));

        // An unreadable block is never dropped.
        let unreadable = merge_declaration(None, &Declaration::Unreadable);
        assert_eq!(
            unreadable.as_deref(),
            Some([UNREADABLE_DECLARATION.to_owned()].as_slice())
        );

        // More distinct crates than the bound keeps the task unverifiable, not half-checked.
        let many: Vec<String> = (0..MAX_DECLARED + 1).map(|i| format!("c{i}")).collect();
        let overflow = merge_declaration(None, &Declaration::Tests(many));
        let kept = overflow.expect("non-empty set");
        assert_eq!(kept.len(), MAX_DECLARED);
        assert_eq!(kept.last().map(String::as_str), Some(UNREADABLE_DECLARATION));
    }

    #[test]
    fn declared_selectors_map_to_entries_or_keep_the_task_unverifiable() {
        let discovered = BTreeMap::from([("one".to_owned(), "".to_owned())]);
        let tested = ["one".to_owned()].into_iter().collect::<BTreeSet<_>>();
        // Already derived: no duplicate entry.
        assert!(declared_entries(&["one".to_owned()], &discovered, &tested)
            .unwrap()
            .is_empty());
        // A named crate gets exactly its test entry.
        assert_eq!(
            declared_entries(&["one".to_owned()], &discovered, &BTreeSet::new()).unwrap(),
            ["in .: command -v cargo >/dev/null 2>&1 || exit 127\ncargo test --frozen -p one".to_owned()]
        );
        // Unknown or unreadable declarations are errors, never silent skips.
        for bad in [
            vec!["missing".to_owned()],
            vec![UNREADABLE_DECLARATION.to_owned()],
            vec!["one".to_owned(), "missing".to_owned()],
        ] {
            let missing = declared_entries(&bad, &discovered, &tested).unwrap_err();
            assert_eq!(missing.len(), 1, "{bad:?}");
            assert!(!missing[0].contains("cargo test"), "{missing:?}");
        }
    }

    #[test]
    fn evidence_classification_separates_verdicts_from_checks_that_never_ran() {
        assert_eq!(classify(Some(0), false), CheckStatus::Passed);
        assert_eq!(classify(Some(1), false), CheckStatus::Failed);
        for never in [Some(126), Some(127), None] {
            assert_eq!(classify(never, false), CheckStatus::NotRun, "{never:?}");
        }
        assert_eq!(classify(Some(0), true), CheckStatus::NotRun);
    }

    #[test]
    fn supervisor_evidence_parses_only_as_a_complete_matching_record() {
        let record = r#"{"type":"oxa_checks","version":1,"checks":[
            {"command":"true","exit":0,"timed_out":false},
            {"command":"failing","exit":3,"timed_out":false},
            {"command":"toolchain","exit":null,"timed_out":false}
        ]}"#;
        let runs = parse_evidence(record, 3).expect("matching evidence");
        assert_eq!(runs[0].status, CheckStatus::Passed);
        assert_eq!(runs[1].status, CheckStatus::Failed);
        assert_eq!(runs[2].status, CheckStatus::NotRun);
        assert_eq!(runs[2].exit, None);

        // Anything else leaves the round without a verdict.
        for broken in [
            r#"{"type":"other","version":1,"checks":[]}"#,
            r#"{"type":"oxa_checks","version":2,"checks":[]}"#,
            r#"{"type":"oxa_checks","version":1,"checks":[{"command":"true","exit":0}]}"#,
            r#"{"type":"oxa_checks","version":1,"checks":[{"exit":0,"timed_out":false}]}"#,
            "not json at all",
        ] {
            assert!(parse_evidence(broken, 1).is_none(), "{broken}");
        }
        // A record that does not name every sent check exactly once is refused.
        assert!(parse_evidence(record, 4).is_none());
    }

    #[test]
    fn evidence_commands_are_bounded_without_splitting_characters() {
        assert_eq!(bounded_command("true"), "true");
        let long = "echo ".to_owned() + &"é".repeat(300);
        let cut = bounded_command(&long);
        assert!(cut.ends_with("…[truncated]"));
        assert!(cut.len() <= COMMAND_CAP + "…[truncated]".len());
        assert!(long.starts_with(cut.trim_end_matches("…[truncated]")));
    }

    #[test]
    fn discovery_lists_the_crates_of_a_tree_with_their_workspace_dirs() {
        let dir = PrivateDir::new("derived-discover");
        manifest(&dir, "Cargo.toml", workspace());
        manifest(&dir, "crates/one/Cargo.toml", "[package]\nname = \"one\"\n");
        manifest(&dir, "tools/pinned/Cargo.toml", "[package]\nname = \"pinned\"\n");
        // Skipped trees: a .git, target and node_modules directory.
        for skip in [".git", "target", "node_modules"] {
            manifest(
                &dir,
                &format!("{skip}/deep/Cargo.toml"),
                "[package]\nname = \"skipped\"\n",
            );
        }
        let found = discover(dir.path()).expect("bounded tree");
        assert_eq!(
            found,
            BTreeMap::from([
                ("one".to_owned(), "".to_owned()),
                ("pinned".to_owned(), "".to_owned()),
            ])
        );
    }
}
