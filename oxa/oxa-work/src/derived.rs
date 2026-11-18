//! Done checks derived from what a task actually changed (#1442): the tests and clippy run
//! of every Cargo crate the worker touched, next to the project's static list, plus crates
//! the worker declared as the task's acceptance tests.
//!
//! Selection is host-side metadata only: `git diff --name-only`/`git ls-files` never read
//! file contents, run filters or repository helpers. Every derived entry is a fixed shell
//! shape over validated crate names, so worker text can never become a check command. A
//! check that could not run (toolchain missing, timeout) is `not_run`, and a task with any
//! such check ends `unverified`, never `done`.
use crate::{checked, git, RunPolicy};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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

/// A selected crate: its name and the workspace directory its cargo command runs in.
#[derive(Debug, PartialEq, Eq)]
struct Package {
    name: String,
    in_dir: String,
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
    head: Option<&str>,
) -> Result<DerivedChecks, String> {
    let paths = changed_paths(policy, root, base, head)?;
    let selected = select(root, &paths)?;
    Ok(commands(&selected))
}

/// Paths that differ from `base`: tracked changes against the working tree (or `head` for a
/// clean export) plus the untracked files the worker created.
fn changed_paths(
    policy: &RunPolicy,
    root: &Path,
    base: &str,
    head: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut diff = vec!["diff", "--name-only", "--no-ext-diff", "--no-textconv", "-z", base];
    if let Some(head) = head {
        diff.push(head);
    }
    diff.push("--");
    let tracked = checked(git(policy, root, &diff))?;
    let mut paths: Vec<String> = tracked
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
    if head.is_none() {
        let untracked = checked(git(
            policy,
            root,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        ))?;
        paths.extend(
            untracked
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned),
        );
    }
    if paths.len() > MAX_CHANGED_PATHS {
        return Err(format!(
            "the change lists more than {MAX_CHANGED_PATHS} paths, so its crates cannot be selected"
        ));
    }
    Ok(paths)
}

/// One parsed `Cargo.toml`: the package name, whether it roots a workspace, and that
/// workspace's member paths (globs allowed; comments and multiline tolerated).
#[derive(Default, Debug, PartialEq, Eq)]
struct Manifest {
    package: Option<String>,
    workspace: bool,
    members: Vec<String>,
}

/// Read the manifest of `dir` (relative to `root`; "" is the tree root). `None`: absent,
/// too large, or not readable. A broken manifest selects nothing, like a project without
/// Cargo; the derived set then still contains whatever else the change touched.
fn read_manifest(root: &Path, dir: &str) -> Option<Manifest> {
    let path = root.join(join_dir(dir, "Cargo.toml"));
    let bytes = std::fs::read(&path).ok()?;
    if bytes.len() as u64 > MANIFEST_BYTES {
        return None;
    }
    Some(parse_manifest(&String::from_utf8_lossy(&bytes)))
}

/// A line-oriented subset of TOML: section headers, `[package] name`, a `[workspace]` flag
/// and `members` lists. It never fails; an unknown shape reads as an empty manifest.
fn parse_manifest(text: &str) -> Manifest {
    let mut out = Manifest::default();
    let mut section = String::new();
    let mut members = None;
    for line in text.lines() {
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

/// The quoted strings of a `members` value, without comments, in order.
fn quoted_items(value: &str) -> Vec<String> {
    let without_comments: String = value
        .lines()
        .map(|line| match line.find('#') {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut items = Vec::new();
    let mut rest = without_comments.as_str();
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
        && path.split('/').all(|part| !part.is_empty() && part != "..")
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

/// Manifests read so far, keyed by relative directory ("" is the tree root). An unreadable
/// or absent manifest is an empty one: it selects nothing and roots no workspace.
#[derive(Default)]
struct Manifests {
    root: BTreeMap<String, Manifest>,
}

impl Manifests {
    fn at(&mut self, root: &Path, dir: &str) -> &Manifest {
        if !self.root.contains_key(dir) {
            let manifest = read_manifest(root, dir).unwrap_or_default();
            self.root.insert(dir.to_owned(), manifest);
        }
        &self.root[dir]
    }
    /// The directories at and above `dir` with a package or workspace manifest, deepest
    /// first, reading manifests on the way.
    fn ancestors(&mut self, root: &Path, dir: &str) -> Vec<String> {
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
        chain.retain(|dir| {
            let manifest = self.at(root, dir);
            manifest.package.is_some() || manifest.workspace
        });
        chain
    }
    /// The directory a cargo command for the crate at `dir` runs in: the nearest workspace
    /// root at or above it, or the crate directory itself for a standalone crate.
    fn in_dir(&mut self, root: &Path, dir: &str) -> String {
        for ancestor in self.ancestors(root, dir) {
            if self.root[&ancestor].workspace {
                return ancestor;
            }
        }
        dir.to_owned()
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
            let manifest = manifests.at(root, dir);
            if manifest.workspace {
                let package = manifest.package.clone();
                let members = manifest.members.clone();
                for member in members {
                    for member_dir in member_dirs(root, dir, &member) {
                        if let Some(package) = manifests.at(root, &member_dir).package.clone() {
                            selected.insert(package, dir.to_owned());
                        }
                    }
                }
                if let Some(package) = package {
                    selected.insert(package, dir.to_owned());
                }
            } else if name == "Cargo.toml" {
                if let Some(package) = manifest.package.clone() {
                    selected.insert(package, dir.to_owned());
                }
            }
            continue;
        }
        for ancestor in manifests.ancestors(root, dir) {
            if let Some(package) = manifests.root[&ancestor].package.clone() {
                let in_dir = manifests.in_dir(root, &ancestor);
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

/// The tests of one crate. `--frozen` keeps the lockfile and the network out of the
/// worktree; a cold registry or a drifted lockfile is a failure to fix, not a silent pass.
fn test_entry(in_dir: &str, name: &str) -> String {
    format!("in {in_dir}: command -v cargo >/dev/null 2>&1 || exit 127\ncargo test --frozen -p {name}")
}

/// Clippy of one crate, guarded so a missing component is `not_run` (the task stays
/// `unverified`) instead of cargo's generic failure.
fn clippy_entry(in_dir: &str, name: &str) -> String {
    format!("in {in_dir}: cargo clippy --version >/dev/null 2>&1 || exit 127\ncargo clippy --frozen -p {name} -- -D warnings")
}

/// Every crate of the tree, bounded. `None`: the walk exceeded its capacity, so declared
/// selectors cannot be resolved and the task stays `unverified` rather than half-checked.
pub(crate) fn discover(root: &Path) -> Option<BTreeMap<String, String>> {
    let mut manifests = Manifests::default();
    let mut stack = vec![String::new()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let path = if dir.is_empty() { root.to_owned() } else { root.join(&dir) };
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
                let manifest = read_manifest(root, &manifest_dir).unwrap_or_default();
                manifests.root.insert(manifest_dir, manifest);
            }
        }
    }
    let mut packages = BTreeMap::new();
    let dirs: Vec<String> = manifests.root.keys().cloned().collect();
    for dir in dirs {
        if let Some(name) = manifests.root[&dir].package.clone() {
            let in_dir = manifests.in_dir(root, &dir);
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
        return Declaration::Absent;
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
/// must be verified even if a later repair round drops the block.
pub(crate) fn merge_declaration(declared: Option<Vec<String>>, next: &Declaration) -> Option<Vec<String>> {
    let mut merged = declared.unwrap_or_default();
    match next {
        Declaration::Absent => {}
        Declaration::Tests(selectors) => merged.extend(selectors.iter().cloned()),
        Declaration::Unreadable => merged.push(UNREADABLE_DECLARATION.to_owned()),
    }
    merged.truncate(MAX_DECLARED);
    (!merged.is_empty()).then_some(merged)
}

/// The per-check evidence a check supervisor reports on its stdout. `None`: no complete,
/// matching record, which keeps the outcome `unverified` rather than trusting a bare exit.
pub(crate) fn parse_evidence(stdout: &str, expected: usize) -> Option<Vec<CheckRun>> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    if value["type"] != "oxa_checks" || value["version"] != 1 {
        return None;
    }
    let checks = value["checks"].as_array()?;
    if checks.len() != expected {
        return None;
    }
    let mut runs = Vec::with_capacity(checks.len());
    for check in checks {
        let status = match check["status"].as_str()? {
            "passed" => CheckStatus::Passed,
            "failed" => CheckStatus::Failed,
            "not_run" => CheckStatus::NotRun,
            _ => return None,
        };
        let exit = match &check["exit"] {
            serde_json::Value::Null => None,
            value => {
                let code = value.as_i64()?;
                Some(i32::try_from(code).ok()?)
            }
        };
        runs.push(CheckRun {
            origin: CheckOrigin::Settings,
            command: bounded_command(check["command"].as_str()?),
            exit,
            timed_out: check["timed_out"].as_bool()?,
            status,
        });
    }
    Some(runs)
}

/// A command as kept in evidence, with an explicit marker when it was cut.
pub(crate) fn bounded_command(command: &str) -> String {
    if command.len() <= COMMAND_CAP {
        return command.to_owned();
    }
    let end = command.floor_char_boundary(COMMAND_CAP);
    format!("{}…[truncated]", &command[..end])
}
