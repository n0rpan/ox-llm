//! Every environment name the oxa workspace reads or sets in non-test code is in `ENV_TABLE`
//! (or in the short allowlist below, with a reason), and every user-facing row is in
//! `oxa/docs/pages/cli.md`.

use oxa_config::{EnvKind, ENV_TABLE};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Names that are read or set in non-test code but are not oxa configuration. Each has a reason.
const ALLOWED: &[(&str, &str)] = &[
    (
        "CARGO_TARGET_DIR",
        "toolchain: an owner-set target directory wins over the derived worker build cache",
    ),
    ("HOME", "OS: the user's home directory"),
    ("PATH", "OS: executable search path, passed on to children"),
    ("USER", "OS: login name, for the report footer"),
    ("LOGNAME", "OS: login name, for the report footer"),
    (
        "XDG_RUNTIME_DIR",
        "OS: runtime directory, bound into the bash sandbox",
    ),
    (
        "XDG_CONFIG_HOME",
        "OS: config directory, to find the GitHub CLI's hosts.yml for host-output redaction",
    ),
    ("CARGO", "build tool: cargo path in the parity harness"),
    ("OXA_BIN", "parity harness: path of the oxa binary under test"),
    (
        "OXA_FAKE_BIN",
        "parity harness: path of the fake server binary under test",
    ),
    (
        "OXA_REQUIRE_BWRAP",
        "test marker: make bwrap-dependent tests fail instead of skip",
    ),
    ("GH_TOKEN", "gh CLI: token oxa work passes to or strips from gh"),
    ("GH_ENTERPRISE_TOKEN", "gh CLI: stripped from worker environments"),
    ("GITHUB_TOKEN", "gh CLI: stripped from worker environments"),
    (
        "GITHUB_ENTERPRISE_TOKEN",
        "gh CLI: stripped from worker environments",
    ),
    ("GH_HOST", "gh CLI: host for the report and board probes"),
    (
        "GH_CONFIG_DIR",
        "gh CLI: configuration directory, removed for workers",
    ),
    ("GH_DEBUG", "gh CLI: cleared for the board probe"),
    (
        "GH_NO_UPDATE_NOTIFIER",
        "gh CLI: silences the update notice in probes",
    ),
    (
        "GH_PROMPT_DISABLED",
        "gh CLI: no prompts in probes and publishing",
    ),
    (
        "GIT_ALLOW_PROTOCOL",
        "git: restricts transports for publishing and workers",
    ),
    (
        "GIT_CEILING_DIRECTORIES",
        "git: repository discovery limit in the sandbox",
    ),
    (
        "GIT_CONFIG_GLOBAL",
        "git: pinned or emptied for sandboxed and committed-tree git runs",
    ),
    (
        "GIT_CONFIG_NOSYSTEM",
        "git: no system configuration in the sandbox",
    ),
    ("GIT_CONFIG_SYSTEM", "git: pinned for committed-tree git runs"),
    ("GIT_INDEX_FILE", "git: private index for committed-tree git runs"),
    ("GIT_NO_LAZY_FETCH", "git: no lazy fetch in worker git runs"),
    (
        "GIT_NO_REPLACE_OBJECTS",
        "git: no replace objects in worker git runs",
    ),
    (
        "GIT_OPTIONAL_LOCKS",
        "git: no optional locks for read-only status runs",
    ),
    ("GIT_TERMINAL_PROMPT", "git: never prompt on a terminal"),
];

/// Test-only markers by shape: (prefix, suffix, reason).
const ALLOWED_SHAPES: &[(&str, &str, &str)] = &[
    ("OXA_TEST_", "", "test hooks set by test harnesses"),
    (
        "OXA_STEERING_TEST_",
        "",
        "test child markers of the steering tests",
    ),
    ("OXA_", "_CHILD", "test child-process markers"),
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "tests" && name != "target" {
                rust_files(&path, out);
            }
        } else if name.ends_with(".rs") && name != "tests.rs" {
            out.push(path);
        }
    }
}

/// The text before the first inline `#[cfg(test)] mod name {`. An out-of-line
/// `#[cfg(test)] mod tests;` does not end the file.
fn non_test_part(src: &str) -> &str {
    let mut from = 0;
    while let Some(at) = src[from..].find("#[cfg(test)]") {
        let start = from + at;
        let rest = src[start + "#[cfg(test)]".len()..].trim_start();
        let rest = rest.strip_prefix("pub ").map_or(rest, str::trim_start);
        if let Some(after) = rest.strip_prefix("mod ") {
            let head: String = after.chars().take_while(|c| *c != ';' && *c != '{').collect();
            if after[head.len()..].starts_with('{') {
                return &src[..start];
            }
        }
        from = start + 1;
    }
    src
}

fn is_ox_name(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() > 4
        && b[0] == b'O'
        && b[1] == b'X'
        && b[2].is_ascii_uppercase()
        && b[3] == b'_'
        && b[4..]
            .iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// Every double-quoted literal in `src` that is an `OX?_*` name.
fn ox_literals(src: &str, found: &mut Vec<String>) {
    let mut rest = src;
    while let Some(at) = rest.find("\"OX") {
        let body = &rest[at + 1..];
        if let Some(end) = body.find('"') {
            if is_ox_name(&body[..end]) {
                found.push(body[..end].to_string());
            }
        }
        rest = &rest[at + 1..];
    }
}

/// The literal first argument of each call of `needle`.
fn call_literals(src: &str, needle: &str, found: &mut Vec<String>) {
    let mut rest = src;
    while let Some(at) = rest.find(needle) {
        rest = &rest[at + needle.len()..];
        if let Some(body) = rest.trim_start().strip_prefix('"') {
            if let Some(end) = body.find('"') {
                found.push(body[..end].to_string());
            }
        }
    }
}

fn allowed(name: &str) -> bool {
    ALLOWED.iter().any(|(n, _)| *n == name)
        || ALLOWED_SHAPES
            .iter()
            .any(|(p, s, _)| name.starts_with(p) && name.ends_with(s))
}

#[test]
fn every_name_read_in_source_is_in_env_table() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut crates: Vec<PathBuf> = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.path().join("src"))
        .filter(|p| p.is_dir())
        .collect();
    crates.sort();
    assert!(crates.len() > 10, "found the workspace crates under {root:?}");
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for src_dir in crates {
        let mut files = Vec::new();
        rust_files(&src_dir, &mut files);
        for file in files {
            let text = fs::read_to_string(&file).unwrap();
            let text = non_test_part(&text);
            let mut names = Vec::new();
            ox_literals(text, &mut names);
            for needle in ["env::var(", "env::var_os(", "env.get(", ".env(", ".env_remove("] {
                call_literals(text, needle, &mut names);
            }
            for name in names {
                seen.entry(name).or_insert_with(|| file.display().to_string());
            }
        }
    }
    let missing: Vec<String> = seen
        .iter()
        .filter(|(name, _)| !ENV_TABLE.iter().any(|row| row.name == name.as_str()) && !allowed(name))
        .map(|(name, file)| format!("{name} (first seen in {file})"))
        .collect();
    assert!(
        missing.is_empty(),
        "environment names read or set in non-test code but not in ENV_TABLE \
         (oxa-config/src/env.rs) or the allowlist of this test: {missing:#?}"
    );
}

#[test]
fn every_user_and_set_row_is_in_cli_md() {
    let page =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/pages/cli.md")).unwrap();
    let missing: Vec<&str> = ENV_TABLE
        .iter()
        .filter(|row| matches!(row.kind, EnvKind::User | EnvKind::Set))
        .filter(|row| !page.contains(&format!("`{}`", row.name)))
        .map(|row| row.name)
        .collect();
    assert!(missing.is_empty(), "rows missing from cli.md: {missing:?}");
    let internal_missing: Vec<&str> = ENV_TABLE
        .iter()
        .filter(|row| row.kind == EnvKind::Internal)
        .filter(|row| !page.contains(&format!("`{}`", row.name)))
        .map(|row| row.name)
        .collect();
    assert!(
        internal_missing.is_empty(),
        "internal rows missing from the Internal list of cli.md: {internal_missing:?}"
    );
}

#[test]
fn allowlist_entries_have_reasons_and_no_table_duplicates() {
    for (name, reason) in ALLOWED {
        assert!(!reason.is_empty(), "{name}");
        assert!(
            !ENV_TABLE.iter().any(|row| row.name == *name),
            "{name} is in ENV_TABLE: drop it from the allowlist"
        );
    }
}
