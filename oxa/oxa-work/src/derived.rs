//! Local compile checks derived from the Cargo crates a task touched (#1442).
//! Test acceptance depends on the exact-head hosted lane.
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

/// One check the runner ran, with the evidence its verdict rests on (#1442).
/// The command has been scrubbed for display and bounded by [`COMMAND_CAP`].
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

/// Local compile entries and whether their change needs hosted test evidence.
pub(crate) struct DerivedChecks {
    /// Full `in <dir>: <command>` entries, sorted for stable evidence.
    pub commands: Vec<String>,
    /// A Rust change needs hosted test evidence even when the local list is empty.
    pub requires_hosted: bool,
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
const MANIFEST_PLAN_BYTES: u64 = MANIFEST_BYTES * MAX_PACKAGES as u64;
const MAX_WALK_ENTRIES: usize = 20_000;
const MAX_GLOB_WORK: usize = 4_000_000;
const DEFAULT_COMPILE_COMMAND: &str = "cargo check";
/// Bytes of one check command kept in evidence.
pub(crate) const COMMAND_CAP: usize = 512;

/// Exit statuses meaning the check never executed: the executable, component or toolchain
/// was missing. Any other status is a real verdict about the change.
pub(crate) const NOT_RUN_EXIT: &[i32] = &[126, 127];

/// Commands approved by the owner for the local compile precheck. Only a
/// restricted Cargo check invocation is accepted. The runner supplies the crate.
#[derive(Clone, Debug)]
pub struct LocalCheckCommands(Vec<String>);

impl Default for LocalCheckCommands {
    fn default() -> Self {
        Self(vec![DEFAULT_COMPILE_COMMAND.to_owned()])
    }
}

impl LocalCheckCommands {
    pub fn from_owner_setting(setting: Option<&serde_json::Value>) -> Result<Self, String> {
        let Some(setting) = setting else {
            return Ok(Self::default());
        };
        let values = setting
            .as_array()
            .ok_or("oxa.work.localChecks must be a list of cargo check commands")?;
        let mut commands = Vec::new();
        for value in values {
            let mut words = value
                .as_str()
                .ok_or("oxa.work.localChecks entries must be strings")?
                .split_ascii_whitespace();
            if words.next() != Some("cargo") || words.next() != Some("check") {
                return Err("oxa.work.localChecks permits only cargo check commands".into());
            }
            let mut flags = BTreeSet::new();
            for flag in words {
                if !matches!(
                    flag,
                    "--all-targets"
                        | "--all-features"
                        | "--no-default-features"
                        | "--locked"
                        | "--offline"
                        | "--frozen"
                ) {
                    return Err("oxa.work.localChecks contains an unsupported cargo check option".into());
                }
                flags.insert(flag);
            }
            let mut command = DEFAULT_COMPILE_COMMAND.to_owned();
            for flag in flags {
                command.push(' ');
                command.push_str(flag);
            }
            if !commands.contains(&command) {
                commands.push(command);
            }
        }
        Ok(Self(commands))
    }
}

/// Whether this exact committed change needs hosted test evidence.
pub(crate) fn committed_requires_hosted(
    policy: &RunPolicy,
    root: &Path,
    base: &str,
    head: &str,
) -> Result<bool, String> {
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
    Ok(select_changed_paths(root, &diff.stdout)?.requires_hosted)
}

#[cfg(test)]
fn from_changed_paths(root: &Path, paths: &[u8]) -> Result<DerivedChecks, String> {
    from_changed_paths_with(root, paths, &LocalCheckCommands::default())
}

pub(crate) fn from_changed_paths_with(
    root: &Path,
    paths: &[u8],
    local_checks: &LocalCheckCommands,
) -> Result<DerivedChecks, String> {
    let selected = select_changed_paths(root, paths)?;
    Ok(DerivedChecks {
        commands: commands(&selected.packages, local_checks),
        requires_hosted: selected.requires_hosted,
    })
}

fn select_changed_paths(root: &Path, paths: &[u8]) -> Result<Selected, String> {
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
    select(root, &paths)
}

/// One parsed `Cargo.toml`: the package name, whether it roots a workspace, and that
/// workspace's member paths (globs allowed; comments and multiline tolerated).
#[derive(Default, Debug, PartialEq, Eq)]
struct Manifest {
    present: bool,
    package: Option<String>,
    workspace: bool,
    members: Vec<String>,
    excludes: Vec<String>,
    unsupported_workspace_list: bool,
}

struct ManifestBudget {
    files_left: usize,
    bytes_left: u64,
}

impl Default for ManifestBudget {
    fn default() -> Self {
        Self {
            files_left: MAX_MANIFESTS,
            bytes_left: MANIFEST_PLAN_BYTES,
        }
    }
}

/// Read a bounded regular manifest beneath the checkout. Walk by descriptor so
/// project-controlled symlinks cannot redirect host-side reads.
fn read_manifest(root: &Path, dir: &str, budget: &mut ManifestBudget) -> Result<Option<Manifest>, String> {
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
        if budget.files_left == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "aggregate manifest count exhausted",
            ));
        }
        if metadata.len() > budget.bytes_left {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "aggregate manifest byte budget exhausted",
            ));
        }
        let limit = MANIFEST_BYTES.min(budget.bytes_left);
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MANIFEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "manifest exceeds the read limit",
            ));
        }
        if bytes.len() as u64 > budget.bytes_left {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "aggregate manifest byte budget exhausted",
            ));
        }
        Ok(bytes)
    };

    match read() {
        Ok(bytes) => {
            budget.files_left -= 1;
            budget.bytes_left -= bytes.len() as u64;
            let path = join_dir(dir, "Cargo.toml");
            let text = std::str::from_utf8(&bytes).map_err(|_| format!("{path} is not UTF-8"))?;
            let mut manifest = parse_manifest(text);
            if manifest.unsupported_workspace_list
                || manifest.members.len() > MAX_MEMBERS
                || manifest.excludes.len() > MAX_MEMBERS
                || manifest
                    .members
                    .iter()
                    .chain(&manifest.excludes)
                    .any(|pattern| pattern.contains(['[', ']']))
            {
                return Err(format!("unsupported or incomplete workspace path list in {path}"));
            }
            manifest.present = true;
            Ok(Some(manifest))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", join_dir(dir, "Cargo.toml"))),
    }
}

/// A line-oriented subset of TOML: section headers, `[package] name`, a `[workspace]` flag
/// and workspace path lists. Unknown sections do not contribute acceptance commands.
fn parse_manifest(text: &str) -> Manifest {
    let mut out = Manifest::default();
    let mut section = String::new();
    let mut list = None::<(bool, String)>;
    for line in text.lines() {
        let line = manifest_line(line);
        if let Some((exclude, buffer)) = &mut list {
            buffer.push(' ');
            buffer.push_str(line);
            if unquoted_delimiter(line, ']').is_some() {
                let paths = if *exclude {
                    &mut out.excludes
                } else {
                    &mut out.members
                };
                match quoted_items(buffer) {
                    Some(items) => *paths = items,
                    None => out.unsupported_workspace_list = true,
                }
                list = None;
            }
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('[') {
            let table = rest.split(']').next().unwrap_or(rest).trim();
            section = unquote(table).unwrap_or(table).to_owned();
            if section == "workspace" || section.starts_with("workspace.") {
                out.workspace = true;
            }
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            let (key, value) = (key.trim(), value.trim());
            let key = unquote(key).unwrap_or(key);
            if section == "package" && out.package.is_none() && key == "name" {
                out.package = unquote(value)
                    .filter(|name| valid_package_name(name))
                    .map(str::to_owned);
            }
            if section == "workspace" && matches!(key, "members" | "exclude") {
                let exclude = key == "exclude";
                if unquoted_delimiter(value, ']').is_some() {
                    let paths = if exclude {
                        &mut out.excludes
                    } else {
                        &mut out.members
                    };
                    match quoted_items(value) {
                        Some(items) => *paths = items,
                        None => out.unsupported_workspace_list = true,
                    }
                } else {
                    list = Some((exclude, value.to_owned()));
                }
            }
        }
    }
    out.unsupported_workspace_list |= list.is_some();
    out.members.sort();
    out.members.dedup();
    out.excludes.sort();
    out.excludes.dedup();
    out
}

/// Remove a TOML comment before folding a line into a multiline members list.
fn manifest_line(line: &str) -> &str {
    &line[..unquoted_delimiter(line, '#').unwrap_or(line.len())]
}

fn unquoted_delimiter(line: &str, delimiter: char) -> Option<usize> {
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
                ch if ch == delimiter => return Some(index),
                '"' | '\'' => quote = Some(ch),
                _ => {}
            }
        }
    }
    None
}

/// The quoted strings of a `members` value, without comments, in order.
fn quoted_items(value: &str) -> Option<Vec<String>> {
    let mut items = Vec::new();
    let mut rest = value.trim().strip_prefix('[')?.trim();
    loop {
        if rest == "]" {
            return Some(items);
        }
        let quote = rest.chars().next().filter(|ch| matches!(ch, '"' | '\''))?;
        let end = rest[1..].find(quote)? + 1;
        let item = rest[1..end].trim_start_matches("./");
        if !valid_member_path(item) || item.contains('\\') {
            return None;
        }
        items.push(item.to_owned());
        rest = rest[end + 1..].trim();
        if rest != "]" {
            rest = rest.strip_prefix(',')?.trim();
        }
    }
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

fn unquote(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    let quote = *bytes.first()?;
    if (quote == b'"' || quote == b'\'') && bytes.last() == Some(&quote) && bytes.len() >= 2 {
        return Some(&value[1..value.len() - 1]);
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
    budget: ManifestBudget,
}

impl Manifests {
    fn at(&mut self, root: &Path, dir: &str) -> Result<&Manifest, String> {
        if !self.root.contains_key(dir) {
            if self.root.len() >= MAX_WALK_ENTRIES {
                return Err("aggregate manifest lookup budget exhausted".into());
            }
            let manifest = read_manifest(root, dir, &mut self.budget)?.unwrap_or_default();
            self.root.insert(dir.to_owned(), manifest);
        }
        Ok(&self.root[dir])
    }
    /// The manifest-bearing directories at and above `dir`, deepest first.
    fn ancestors(&mut self, root: &Path, dir: &str) -> Result<Vec<String>, String> {
        // Apply the manifest reader's existing path limit before expanding ancestors.
        if !dir.is_empty() && !valid_member_path(dir) {
            return Err("changed file is outside the supported manifest directory format".into());
        }
        let mut found = Vec::new();
        let mut current = dir;
        loop {
            let manifest = self.at(root, current)?;
            if manifest.present {
                found.push(current.to_owned());
            }
            if current.is_empty() {
                return Ok(found);
            }
            current = parent_dir(current);
        }
    }
    /// The directory a cargo command for the crate at `dir` runs in: the nearest workspace
    /// root at or above it, or the crate directory itself for a standalone crate.
    fn in_dir(&mut self, root: &Path, dir: &str, budget: &mut usize) -> Result<String, String> {
        for ancestor in self.ancestors(root, dir)? {
            let manifest = &self.root[&ancestor];
            if manifest.workspace {
                if workspace_excludes(&ancestor, dir, &manifest.excludes, budget)? {
                    return Ok(dir.to_owned());
                }
                return Ok(ancestor);
            }
        }
        Ok(dir.to_owned())
    }
}

/// The crates of the changed paths. Only a concrete package selects a local
/// check. The hosted lane handles workspace-wide metadata changes.
#[derive(Debug, Default)]
struct Selected {
    packages: BTreeSet<(String, String)>,
    requires_hosted: bool,
}

fn select(root: &Path, paths: &[String]) -> Result<Selected, String> {
    let mut manifests = Manifests::default();
    let mut selected = Selected::default();
    let mut budget = MAX_GLOB_WORK;
    for path in paths {
        let name = path.rsplit('/').next().unwrap_or(path.as_str());
        let dir = parent_dir(path);
        // Shared Cargo inputs can change the build without selecting a package.
        if matches!(name, "rust-toolchain" | "rust-toolchain.toml")
            || dir.split('/').any(|part| part == ".cargo")
        {
            selected.requires_hosted = true;
            continue;
        }
        // A missing or unsupported package declaration cannot make a Rust edit verified.
        selected.requires_hosted |= name.ends_with(".rs");
        if name == "Cargo.toml" || name == "Cargo.lock" {
            selected.requires_hosted = true;
            let manifest = manifests.at(root, dir)?;
            if let Some(package) = manifest.package.clone() {
                selected
                    .packages
                    .insert((package, manifests.in_dir(root, dir, &mut budget)?));
            }
            continue;
        }
        for ancestor in manifests.ancestors(root, dir)? {
            let manifest = &manifests.root[&ancestor];
            // Unmapped manifests can still own build inputs that need hosted tests.
            selected.requires_hosted = true;
            if let Some(package) = manifest.package.clone() {
                let in_dir = manifests.in_dir(root, &ancestor, &mut budget)?;
                selected.packages.insert((package, in_dir));
                break;
            }
        }
    }
    if !selected.requires_hosted && !paths.is_empty() {
        // Cargo inputs can sit outside every manifest directory. A bounded search
        // must establish that the tree has no Cargo manifests before a local-only
        // verdict is allowed. There is still no local Cargo target for these paths.
        selected.requires_hosted = find_manifest_dir(root, true, |_| Some(true)).unwrap_or(true);
    }
    if selected.packages.len() > MAX_PACKAGES {
        return Err(format!(
            "the change affects more than {MAX_PACKAGES} Cargo targets, so their checks cannot be listed"
        ));
    }
    Ok(selected)
}

fn spend_glob_work(budget: &mut usize, cost: usize) -> Result<(), String> {
    *budget = budget
        .checked_sub(cost)
        .ok_or("workspace path matching exceeds its work limit")?;
    Ok(())
}

fn match_component(pattern: &str, name: &str, budget: &mut usize) -> Result<bool, String> {
    if !pattern.contains(['*', '?']) {
        spend_glob_work(budget, 1)?;
        return Ok(pattern == name);
    }
    spend_glob_work(budget, (pattern.len() + 1).saturating_mul(name.len() + 1))?;
    Ok(simple_glob(pattern, name))
}

// Match a package directory using the same component-level syntax as member expansion.
fn workspace_excludes(
    workspace: &str,
    dir: &str,
    patterns: &[String],
    budget: &mut usize,
) -> Result<bool, String> {
    for pattern in patterns {
        let full = join_dir(workspace, pattern);
        let mut names = dir.split('/');
        let mut matched = true;
        for part in full.split('/') {
            let Some(name) = names.next() else {
                matched = false;
                break;
            };
            if !match_component(part, name, budget)? {
                matched = false;
                break;
            }
        }
        if matched && names.next().is_none() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `*` and `?` only, anchored at both ends; member globs never need character classes.
fn simple_glob(pattern: &str, name: &str) -> bool {
    // Track which name prefixes match the preceding pattern. Each pattern
    // byte visits each prefix once, even when several stars could match it.
    let mut prefixes = vec![false; name.len() + 1];
    prefixes[0] = true;
    for byte in pattern.bytes() {
        if byte == b'*' {
            for index in 1..prefixes.len() {
                prefixes[index] |= prefixes[index - 1];
            }
        } else {
            for (index, candidate) in name.bytes().enumerate().rev() {
                prefixes[index + 1] = prefixes[index] && (byte == b'?' || byte == candidate);
            }
            prefixes[0] = false;
        }
    }
    prefixes[name.len()]
}

/// One local compile check for each selected crate.
fn commands(selected: &BTreeSet<(String, String)>, local_checks: &LocalCheckCommands) -> Vec<String> {
    let mut out = Vec::new();
    for (name, in_dir) in selected {
        for command in &local_checks.0 {
            out.push(check_entry(in_dir, command, name));
        }
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

fn check_entry(in_dir: &str, command: &str, name: &str) -> String {
    format!(
        "in {}: cargo --version >/dev/null || exit 127\n{command} -p {name}",
        entry_dir(in_dir)
    )
}

/// Visit manifest directories until the predicate matches or the bounded walk finishes.
/// An incomplete walk returns `None`.
fn find_manifest_dir(
    root: &Path,
    include_generated_dirs: bool,
    mut matches: impl FnMut(&str) -> Option<bool>,
) -> Option<bool> {
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
            if seen > MAX_WALK_ENTRIES {
                return None;
            }
            let entry = entry.ok()?;
            let file_type = entry.file_type().ok()?;
            let file_name = entry.file_name();
            let Some(file) = file_name.to_str() else {
                if file_type.is_dir() && include_generated_dirs {
                    return None;
                }
                continue;
            };
            if file_type.is_dir() {
                if file != ".git" && (include_generated_dirs || !matches!(file, "target" | "node_modules")) {
                    stack.push(join_dir(&dir, file));
                }
            } else if file == "Cargo.toml" && matches(&dir)? {
                return Some(true);
            }
        }
    }
    Some(false)
}

/// List the tree's Cargo packages. An incomplete walk cannot resolve declared selectors.
pub(crate) fn discover(root: &Path) -> Option<BTreeSet<(String, String)>> {
    let mut manifests = Manifests::default();
    find_manifest_dir(root, false, |dir| {
        let manifest = read_manifest(root, dir, &mut manifests.budget).ok()??;
        manifests.root.insert(dir.to_owned(), manifest);
        Some(false)
    })?;
    let mut packages = BTreeSet::new();
    let mut budget = MAX_GLOB_WORK;
    let dirs: Vec<String> = manifests.root.keys().cloned().collect();
    for dir in dirs {
        if let Some(name) = manifests.root[&dir].package.clone() {
            let in_dir = manifests.in_dir(root, &dir, &mut budget).ok()?;
            packages.insert((name, in_dir));
        }
    }
    Some(packages)
}

/// Resolve declared acceptance crates. They are requirements for hosted tests.
pub(crate) fn validate_declarations(
    declared: &[String],
    discovered: &BTreeSet<(String, String)>,
) -> Result<(), Vec<String>> {
    let mut missing = Vec::new();
    for selector in declared {
        if selector == UNREADABLE_DECLARATION {
            missing.push("the declared acceptance block was not readable crate names".to_owned());
            continue;
        }
        let mut dirs = discovered
            .iter()
            .filter_map(|(name, dir)| (name == selector).then_some(dir));
        match (dirs.next(), dirs.next()) {
            (Some(_), Some(_)) => missing.push(format!(
                "the declared acceptance crate {selector} is ambiguous across workspaces"
            )),
            (Some(_), None) => {}
            (None, _) => missing.push(format!(
                "the declared acceptance crate {selector} is not a crate of this repository"
            )),
        }
    }
    if missing.is_empty() {
        Ok(())
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
        assert_eq!(
            selected.packages,
            BTreeSet::from([("two".to_owned(), "".to_owned())])
        );
    }

    #[test]
    fn a_standalone_crate_selects_itself_as_its_own_in_dir() {
        let dir = PrivateDir::new("derived-standalone");
        manifest(&dir, "solo/Cargo.toml", "[package]\nname = \"solo\"\n");
        let selected = select(dir.path(), &["solo/src/main.rs".to_owned()]).unwrap();
        assert_eq!(
            selected.packages,
            BTreeSet::from([("solo".to_owned(), "solo".to_owned())])
        );
    }

    #[test]
    fn whitespace_in_cargo_table_headers_keeps_the_package_in_the_check_plan() {
        let dir = PrivateDir::new("derived-header-whitespace");
        manifest(&dir, "Cargo.toml", "[ workspace ]\nmembers = [\"one\"]\n");
        manifest(&dir, "one/Cargo.toml", "[ package ]\nname = \"one\"\n");
        let plan = from_changed_paths(dir.path(), b"one/src/lib.rs\0").unwrap();
        assert!(plan.requires_hosted);
        assert_eq!(plan.commands, [check_entry("", "cargo check", "one")]);
    }

    #[test]
    fn quoted_cargo_keys_still_select_a_touched_package() {
        let dir = PrivateDir::new("derived-quoted-keys");
        manifest(&dir, "Cargo.toml", "[\"workspace\"]\n\"members\" = [\"one\"]\n");
        manifest(&dir, "one/Cargo.toml", "[package]\n\"name\" = \"one\"\n");
        let plan = from_changed_paths(dir.path(), b"one/src/lib.rs\0").unwrap();
        assert!(plan.requires_hosted);
        assert_eq!(plan.commands, [check_entry("", "cargo check", "one")]);
    }

    #[test]
    fn an_unmapped_rust_source_still_requires_hosted_test_evidence() {
        let dir = PrivateDir::new("derived-unmapped-rust");
        manifest(&dir, "Cargo.toml", "package.name = \"one\"\n");
        let plan = from_changed_paths(dir.path(), b"src/lib.rs\0").unwrap();
        assert!(plan.requires_hosted);
        assert!(plan.commands.is_empty());
    }

    #[test]
    fn an_unmapped_manifest_still_requires_hosted_evidence_for_its_inputs() {
        let dir = PrivateDir::new("derived-unmapped-manifest");
        manifest(&dir, "app/Cargo.toml", "package.name = \"one\"\n");
        let plan = from_changed_paths(dir.path(), b"app/data/catalog.txt\0").unwrap();
        assert!(plan.requires_hosted);
        assert!(plan.commands.is_empty());

        let outside = from_changed_paths(dir.path(), b"docs/standalone.txt\0").unwrap();
        assert!(outside.requires_hosted);
    }

    #[test]
    fn a_changed_path_outside_manifest_limits_has_a_bounded_diagnostic() {
        let dir = PrivateDir::new("derived-manifest-limit");
        let parent = "src/".repeat(65);
        let path = format!("{parent}lib.rs\0");
        let Err(error) = from_changed_paths(dir.path(), path.as_bytes()) else {
            panic!("accepted a path outside manifest limits");
        };
        assert_eq!(
            error,
            "changed file is outside the supported manifest directory format"
        );
    }

    #[test]
    fn workspace_metadata_does_not_trigger_a_local_workspace_sweep() {
        let dir = PrivateDir::new("derived-members");
        manifest(&dir, "Cargo.toml", "[workspace]\nmembers = [\"./app\"]\n");
        manifest(&dir, "app/Cargo.toml", "[package]\nname = \"app\"\n");
        for changed in ["Cargo.toml", "Cargo.lock"] {
            let metadata = format!("{changed}\0");
            let checks = from_changed_paths(dir.path(), metadata.as_bytes()).unwrap();
            assert!(checks.commands.is_empty(), "{changed}");
            assert!(checks.requires_hosted, "{changed}");
            let with_source = format!("{changed}\0app/src/lib.rs\0");
            let checks = from_changed_paths(dir.path(), with_source.as_bytes()).unwrap();
            assert_eq!(checks.commands, [check_entry("", "cargo check", "app")]);
        }
    }

    #[test]
    fn changed_path_planning_honors_the_existing_manifest_count_limit() {
        let dir = PrivateDir::new("derived-manifest-count");
        let mut paths = Vec::new();
        for index in 0..=MAX_MANIFESTS {
            let path = format!("member-{index}/Cargo.toml");
            manifest(&dir, &path, "[workspace]\nmembers = []\n");
            paths.extend_from_slice(path.as_bytes());
            paths.push(0);
        }
        let Err(error) = from_changed_paths(dir.path(), &paths) else {
            panic!("accepted more than the existing manifest limit");
        };
        assert!(error.contains("aggregate manifest count"), "{error}");
    }

    #[test]
    fn manifest_reads_share_the_byte_budget_and_cache_hits_do_not_spend_it() {
        let dir = PrivateDir::new("derived-manifest-bytes");
        let package = "[package]\nname = \"one\"\n";
        manifest(&dir, "one/Cargo.toml", package);
        manifest(&dir, "two/Cargo.toml", package);
        let mut manifests = Manifests {
            budget: ManifestBudget {
                files_left: 2,
                bytes_left: package.len() as u64,
            },
            ..Manifests::default()
        };
        for _ in 0..2 {
            assert_eq!(
                manifests.at(dir.path(), "one").unwrap().package.as_deref(),
                Some("one")
            );
        }
        assert_eq!(manifests.budget.files_left, 1);
        let error = manifests.at(dir.path(), "two").unwrap_err();
        assert!(error.contains("aggregate manifest byte budget"), "{error}");
    }

    #[test]
    fn shared_cargo_settings_require_hosted_evidence_without_a_local_sweep() {
        let dir = PrivateDir::new("derived-shared-cargo-settings");
        manifest(&dir, "Cargo.toml", "[workspace]\nmembers = [\"app\"]\n");
        manifest(&dir, "app/Cargo.toml", "[package]\nname = \"app\"\n");
        for changed in [
            ".cargo/config",
            ".cargo/config.toml",
            "rust-toolchain",
            "rust-toolchain.toml",
            "app/.cargo/config.toml",
            "app/rust-toolchain.toml",
        ] {
            let paths = format!("{changed}\0");
            let checks = from_changed_paths(dir.path(), paths.as_bytes()).unwrap();
            assert!(checks.requires_hosted, "{changed}");
            assert!(checks.commands.is_empty(), "{changed}");
        }
        let other = PrivateDir::new("derived-non-cargo-docs");
        let docs = from_changed_paths(other.path(), b"docs/cargo-config.toml\0").unwrap();
        assert!(!docs.requires_hosted);
        assert!(docs.commands.is_empty());
    }

    #[test]
    fn a_shared_input_in_a_virtual_workspace_requires_hosted_checks() {
        let dir = PrivateDir::new("derived-shared-input");
        manifest(&dir, "rust/Cargo.toml", "[workspace]\nmembers = [\"app\"]\n");
        manifest(&dir, "rust/app/Cargo.toml", "[package]\nname = \"app\"\n");
        let checks = from_changed_paths(dir.path(), b"rust/shared/policy.txt\0").unwrap();
        assert!(checks.requires_hosted);
        assert!(checks.commands.is_empty());
        let outside = from_changed_paths(dir.path(), b"shared/policy.txt\0").unwrap();
        assert!(outside.requires_hosted);
        assert!(outside.commands.is_empty());
    }

    #[test]
    fn external_inputs_in_a_repository_with_a_standalone_crate_require_hosted_checks() {
        let rust = PrivateDir::new("derived-standalone-with-shared-inputs");
        manifest(&rust, "app/Cargo.toml", "[package]\nname = \"app\"\n");
        let checks = from_changed_paths(rust.path(), b"shared/catalog.txt\0").unwrap();
        assert!(checks.commands.is_empty());
        assert!(checks.requires_hosted);

        let non_rust = PrivateDir::new("derived-shared-inputs-without-cargo");
        let checks = from_changed_paths(non_rust.path(), b"shared/catalog.txt\0").unwrap();
        assert!(checks.commands.is_empty());
        assert!(!checks.requires_hosted);
    }

    #[test]
    fn an_excluded_member_uses_its_own_check_directory() {
        let dir = PrivateDir::new("derived-excluded");
        manifest(
            &dir,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/solo\"]\n",
        );
        manifest(&dir, "crates/solo/Cargo.toml", "[package]\nname = \"solo\"\n");
        manifest(&dir, "crates/other/Cargo.toml", "[package]\nname = \"other\"\n");
        let changed = select(dir.path(), &["crates/solo/src/lib.rs".to_owned()]).unwrap();
        assert_eq!(
            changed.packages,
            BTreeSet::from([("solo".to_owned(), "crates/solo".to_owned())])
        );
        let root = select(dir.path(), &["Cargo.lock".to_owned()]).unwrap();
        assert!(root.packages.is_empty());
    }

    #[test]
    fn workspace_patterns_are_deduplicated_and_matching_has_a_shared_budget() {
        let parsed = parse_manifest("[workspace]\nexclude = [\"./crates/*\", \"crates/*\"]");
        assert_eq!(parsed.excludes, ["crates/*"]);
        let mut budget = 1;
        let error = workspace_excludes("", "crates/one", &parsed.excludes, &mut budget).unwrap_err();
        assert!(error.contains("work limit"), "{error}");
    }

    #[test]
    fn a_member_class_cannot_silently_produce_partial_workspace_checks() {
        let dir = PrivateDir::new("derived-member-class");
        manifest(
            &dir,
            "Cargo.toml",
            "[workspace]\nmembers = [\n \"crates/[ab]\",\n]\n",
        );
        let error = select(dir.path(), &["Cargo.lock".to_owned()]).unwrap_err();
        assert!(
            error.contains("unsupported or incomplete workspace path list"),
            "{error}"
        );
    }

    #[test]
    fn an_unsupported_member_path_cannot_disappear_from_the_plan() {
        let dir = PrivateDir::new("derived-unsupported-member");
        manifest(
            &dir,
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\", \"../helper\"]\n",
        );
        let error = select(dir.path(), &["Cargo.lock".to_owned()]).unwrap_err();
        assert!(
            error.contains("unsupported or incomplete workspace path list"),
            "{error}"
        );
    }

    #[test]
    fn a_standalone_lockfile_change_selects_its_package() {
        let dir = PrivateDir::new("derived-standalone-lock");
        manifest(&dir, "solo/Cargo.toml", "[package]\nname = \"solo\"\n");
        let selected = select(dir.path(), &["solo/Cargo.lock".to_owned()]).unwrap();
        assert_eq!(
            selected.packages,
            BTreeSet::from([("solo".to_owned(), "solo".to_owned())])
        );
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
        assert!(selected.packages.is_empty());
    }

    #[test]
    fn a_file_of_a_root_package_selects_that_root_crate() {
        let dir = PrivateDir::new("derived-root-package");
        manifest(&dir, "Cargo.toml", root_package_workspace());
        let selected = select(dir.path(), &["docs/page.md".to_owned()]).unwrap();
        assert_eq!(
            selected.packages,
            BTreeSet::from([("top".to_owned(), "".to_owned())])
        );
    }

    #[test]
    fn same_named_packages_in_independent_workspaces_have_distinct_checks() {
        let dir = PrivateDir::new("derived-workspace-identity");
        for workspace in ["a", "b"] {
            manifest(
                &dir,
                &format!("{workspace}/Cargo.toml"),
                "[workspace]\nmembers = [\"util\"]\n",
            );
            manifest(
                &dir,
                &format!("{workspace}/util/Cargo.toml"),
                "[package]\nname = \"util\"\n",
            );
        }
        let paths = ["a/util/src/lib.rs".to_owned(), "b/util/src/lib.rs".to_owned()];
        let checks = commands(
            &select(dir.path(), &paths).unwrap().packages,
            &LocalCheckCommands::default(),
        );
        assert_eq!(
            checks,
            [
                check_entry("a", "cargo check", "util"),
                check_entry("b", "cargo check", "util")
            ]
        );
        let discovered = discover(dir.path()).unwrap();
        let error = validate_declarations(&["util".to_owned()], &discovered).unwrap_err();
        assert!(error[0].contains("ambiguous"), "{error:?}");
    }

    #[test]
    fn derived_entries_are_a_fixed_cargo_shape_per_crate() {
        let checks = commands(
            &BTreeSet::from([
                ("one".to_owned(), "".to_owned()),
                ("solo".to_owned(), "solo".to_owned()),
            ]),
            &LocalCheckCommands::default(),
        );
        assert_eq!(
            checks,
            [
                "in .: cargo --version >/dev/null || exit 127\ncargo check -p one",
                "in solo: cargo --version >/dev/null || exit 127\ncargo check -p solo",
            ]
        );
    }

    #[test]
    fn owner_check_settings_allow_compile_commands_and_can_disable_the_local_plan() {
        let dir = PrivateDir::new("derived-owner-checks");
        manifest(&dir, "Cargo.toml", "[package]\nname = \"one\"\n");
        let configured = serde_json::json!(["cargo check --locked", "cargo check --offline"]);
        let checks = LocalCheckCommands::from_owner_setting(Some(&configured)).unwrap();
        let plan = from_changed_paths_with(dir.path(), b"src/lib.rs\0", &checks).unwrap();
        assert_eq!(
            plan.commands,
            [
                check_entry("", "cargo check --locked", "one"),
                check_entry("", "cargo check --offline", "one")
            ]
        );
        let empty = serde_json::json!([]);
        let checks = LocalCheckCommands::from_owner_setting(Some(&empty)).unwrap();
        let plan = from_changed_paths_with(dir.path(), b"src/lib.rs\0", &checks).unwrap();
        assert!(plan.commands.is_empty());
        assert!(plan.requires_hosted);
        for command in ["cargo test", "cargo clippy", "cargo check --workspace"] {
            let setting = serde_json::json!([command]);
            assert!(
                LocalCheckCommands::from_owner_setting(Some(&setting)).is_err(),
                "{command}"
            );
        }
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
    fn declarations_validate_as_hosted_requirements() {
        let discovered = BTreeSet::from([("one".to_owned(), "".to_owned())]);
        validate_declarations(&["one".to_owned()], &discovered).unwrap();
        // Unknown or unreadable declarations are errors, never silent skips.
        for bad in [
            vec!["missing".to_owned()],
            vec![UNREADABLE_DECLARATION.to_owned()],
            vec!["one".to_owned(), "missing".to_owned()],
        ] {
            let missing = validate_declarations(&bad, &discovered).unwrap_err();
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
            BTreeSet::from([
                ("one".to_owned(), "".to_owned()),
                ("pinned".to_owned(), "".to_owned()),
            ])
        );
    }
}
