//! F11 acceptance tests: Docs. Owner: task F11.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::{parse_script, FakeOpts, FakeServer, TestEnv};
use regex::Regex;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Per doc command: the wall-clock limit before the child is killed.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

// ----- helpers -------------------------------------------------------------------------------

/// The repository root (`CARGO_MANIFEST_DIR/..`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("repo root")
}

fn collect_md(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_md(&p, found);
        } else if p.extension().is_some_and(|x| x == "md") {
            found.push(p);
        }
    }
}

/// The Markdown files of `docs/` (any depth) plus the root files in `extra` that exist, sorted,
/// as (path relative to the repo root, absolute path).
fn md_files(root: &Path, extra: &[&str]) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    collect_md(&root.join("docs"), &mut found);
    for name in extra {
        let p = root.join(name);
        if p.is_file() {
            found.push(p);
        }
    }
    found.sort();
    found
        .into_iter()
        .map(|p| {
            let rel = p.strip_prefix(root).unwrap_or(&p).display().to_string();
            (rel, p)
        })
        .collect()
}

/// A fenced code block: the info string after the opening fence, the line of that fence, and
/// the body lines with their 1-based numbers.
struct Block {
    info: String,
    lines: Vec<(usize, String)>,
}

fn fenced_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut open: Option<Block> = None;
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        let t = line.trim_start();
        if open.is_some() {
            if t.starts_with("```") && t[3..].trim().is_empty() {
                blocks.push(open.take().expect("open block"));
            } else {
                open.as_mut()
                    .expect("open block")
                    .lines
                    .push((n, line.to_string()));
            }
        } else if let Some(info) = t.strip_prefix("```") {
            open = Some(Block {
                info: info.trim().to_string(),
                lines: Vec::new(),
            });
        }
    }
    blocks
}

/// Split a command line into arguments: spaces separate, double quotes group, and a backslash
/// escapes the next character (outside and inside quotes).
fn split_args(line: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut quoted = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                let next = chars.next().ok_or("a trailing backslash")?;
                cur.push(next);
                have = true;
            }
            '"' => {
                quoted = !quoted;
                have = true;
            }
            c if c.is_whitespace() && !quoted => {
                if have {
                    args.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                cur.push(c);
                have = true;
            }
        }
    }
    if quoted {
        return Err("an unterminated double quote".to_string());
    }
    if have {
        args.push(cur);
    }
    Ok(args)
}

/// Run `oxa <args>` (the built binary) against a fresh fake engine in a fresh `TestEnv`, with
/// exactly `TestEnv::envs` as its environment and stdin at null. Err(reason) unless exit 0.
fn run_doc_command(args: &[String]) -> Result<(), String> {
    let env = TestEnv::new();
    // Enough plain text replies for any single prompt in a doc; each command gets its own fake.
    let turns: String = (0..8).map(|_| "{\"text\":\"ok\"}\n").collect();
    let script = parse_script(&turns).expect("script");
    let server = FakeServer::start(script, FakeOpts::default()).expect("start fake engine");
    fs::create_dir_all(env.tmp()).expect("tmp dir");
    let out_file = File::create(env.tmp().join("stdout")).expect("stdout file");
    let err_file = File::create(env.tmp().join("stderr")).expect("stderr file");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args(args)
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(&server.url()))
        .stdin(Stdio::null())
        .stdout(out_file)
        .stderr(err_file)
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("wait: {e}"))? {
            break status;
        }
        if start.elapsed() > COMMAND_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {} s", COMMAND_TIMEOUT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if status.code() == Some(0) {
        return Ok(());
    }
    let stderr = fs::read_to_string(env.tmp().join("stderr")).unwrap_or_default();
    Err(format!("exit {:?}; stderr:\n{stderr}", status.code()))
}

/// The rows of PARITY.md as their five cells (escaped `\|` kept as `|`), id first.
fn parity_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        let Some(l) = l.strip_prefix('|').and_then(|l| l.strip_suffix('|')) else {
            continue;
        };
        let cells: Vec<String> = l
            .replace("\\|", "\u{1}")
            .split('|')
            .map(|c| c.trim().replace('\u{1}', "|"))
            .collect();
        if cells.len() == 5 && is_row_id(&cells[0]) {
            rows.push(cells);
        }
    }
    rows
}

/// `CLI-012`, `ENG-001`: uppercase letters, a dash, digits.
fn is_row_id(s: &str) -> bool {
    match s.split_once('-') {
        Some((p, n)) => {
            !p.is_empty()
                && p.chars().all(|c| c.is_ascii_uppercase())
                && !n.is_empty()
                && n.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    }
}

/// An e-mail address: a local part of letters, digits and `._+-` before an `@`, and a domain
/// with a dot whose last label is alphabetic. `@file` / `@path` (no local part) never match.
fn has_email(line: &str) -> bool {
    let local_ok = |c: char| c.is_ascii_alphanumeric() || "._+-".contains(c);
    let domain_ok = |c: char| c.is_ascii_alphanumeric() || c == '.' || c == '-';
    let chars: Vec<char> = line.chars().collect();
    for (at, c) in chars.iter().enumerate() {
        if *c != '@' {
            continue;
        }
        let mut start = at;
        while start > 0 && local_ok(chars[start - 1]) {
            start -= 1;
        }
        let mut end = at + 1;
        while end < chars.len() && domain_ok(chars[end]) {
            end += 1;
        }
        let local = &chars[start..at];
        let domain: String = chars[at + 1..end].iter().collect();
        let Some((_, tld)) = domain.rsplit_once('.') else {
            continue;
        };
        let tld_ok = tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic());
        if !local.is_empty() && tld_ok && !domain.starts_with('.') {
            return true;
        }
    }
    false
}

/// `O-` followed by three digits, not part of a longer word (an internal decision id).
fn has_internal_id(line: &str) -> bool {
    let b = line.as_bytes();
    (0..b.len()).any(|i| {
        i + 5 <= b.len()
            && b[i] == b'O'
            && b[i + 1] == b'-'
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
            && b[i + 2..i + 5].iter().all(|c| c.is_ascii_digit())
    })
}

// ----- tests ---------------------------------------------------------------------------------

/// fenced `oxa ..` lines extracted and run
#[test]
fn every_doc_command_line_runs_against_fake() {
    let root = repo_root();
    let mut commands = 0;
    let mut failures = Vec::new();
    for (rel, path) in md_files(&root, &["README.md", "SECURITY.md"]) {
        let text = fs::read_to_string(&path).expect("read doc");
        for block in fenced_blocks(&text) {
            if block.info != "sh" {
                continue;
            }
            for (n, raw) in &block.lines {
                let line = raw.trim();
                if line.is_empty() {
                    continue;
                }
                let loc = format!("{rel}:{n}");
                let Some(rest) = line.strip_prefix("oxa ") else {
                    failures.push(format!("{loc}: a sh line must start with `oxa `: {line}"));
                    continue;
                };
                commands += 1;
                let args = match split_args(rest) {
                    Ok(args) => args,
                    Err(why) => {
                        failures.push(format!("{loc}: {why} in `{line}`"));
                        continue;
                    }
                };
                if let Err(why) = run_doc_command(&args) {
                    failures.push(format!("{loc}: `{line}` failed: {why}"));
                }
            }
        }
    }
    if root.join("docs/pages/quickstart.md").exists() {
        assert!(
            commands >= 1,
            "docs/pages/quickstart.md has no `sh` oxa command line"
        );
    }
    assert!(
        failures.is_empty(),
        "{} doc command(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// docs/differences.md is in sync
#[test]
fn difference_table_generated_from_parity_md() {
    let out = Command::new("python3")
        .arg("ci/gen_docs.py")
        .arg("--check")
        .current_dir(repo_root())
        .output()
        .expect("run python3 ci/gen_docs.py --check");
    assert!(
        out.status.success(),
        "python3 ci/gen_docs.py --check failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// no `~/.pi`, `PI_` or `pi ` command in ported pages
#[test]
fn no_pi_names_left() {
    let root = repo_root();
    let pi_env = Regex::new(r"\bPI_[A-Z]").expect("regex");
    let pi_cmd = Regex::new("`pi [a-z-]").expect("regex");
    let mut hits = Vec::new();
    for (rel, path) in md_files(&root, &["SECURITY.md"]) {
        let text = fs::read_to_string(&path).expect("read doc");
        for block in fenced_blocks(&text) {
            for (n, line) in &block.lines {
                if line.trim_start().starts_with("pi ") {
                    hits.push(format!("{rel}:{n}: {line}"));
                }
            }
        }
        for (i, line) in text.lines().enumerate() {
            let n = i + 1;
            // `~/.pi/agent` is named once on purpose: the earlier agent's directory is on the
            // sandbox deny-list (security.md), so the sentence that lists it is allowed.
            let deny_list_entry = line.contains("`~/.pi/agent`");
            let bad = (!deny_list_entry && (line.contains("~/.pi") || line.contains(".pi/")))
                || pi_env.is_match(line)
                || pi_cmd.is_match(line);
            if bad {
                hits.push(format!("{rel}:{n}: {line}"));
            }
        }
    }
    hits.sort();
    hits.dedup();
    assert!(hits.is_empty(), "pi names left in the docs:\n{}", hits.join("\n"));
}

/// O-144: docs/pages/engine-support.md lists every PARITY.md ENG row and every A/O row whose note names O-144
#[test]
fn engine_support_page_in_sync() {
    let root = repo_root();
    let parity = fs::read_to_string(root.join("PARITY.md")).expect("read PARITY.md");
    let mut wanted = Vec::new();
    for cells in parity_rows(&parity) {
        let (id, verdict, note) = (&cells[0], &cells[2], &cells[3]);
        let eng = id.starts_with("ENG-");
        let o144 = (verdict == "A" || verdict == "O") && note.contains("O-144");
        if eng || o144 {
            wanted.push(id.clone());
        }
    }
    assert!(!wanted.is_empty(), "PARITY.md has no ENG or O-144 rows");
    let path = root.join("docs/pages/engine-support.md");
    let page = fs::read_to_string(&path).expect("read docs/pages/engine-support.md");
    let missing: Vec<&String> = wanted
        .iter()
        .filter(|id| !page.contains(&format!("| {id} |")))
        .collect();
    assert!(
        missing.is_empty(),
        "engine-support.md lacks a table row for: {missing:?}"
    );
    let lower = page.to_lowercase();
    for word in [
        "one slot",
        "queue",
        "subagent",
        "text-only",
        "thinking",
        "limits",
        "flash",
    ] {
        assert!(lower.contains(word), "engine-support.md must mention {word:?}");
    }
}

/// no e-mail address and no internal decision id (`O-` + 3 digits) in the docs
#[test]
fn docs_name_no_person_or_internal_id() {
    let root = repo_root();
    let mut hits = Vec::new();
    for (rel, path) in md_files(&root, &["README.md", "SECURITY.md"]) {
        let text = fs::read_to_string(&path).expect("read doc");
        for (i, line) in text.lines().enumerate() {
            if has_email(line) {
                hits.push(format!("{rel}:{}: e-mail address: {line}", i + 1));
            }
            if has_internal_id(line) {
                hits.push(format!("{rel}:{}: internal id: {line}", i + 1));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "person or internal id in the docs:\n{}",
        hits.join("\n")
    );
}

/// No internal id in a user page: work-package ids (W3, H6, EG5, F12, X3 ...), the pi-parity tag
/// `[P]`, decision ids (`O-` + digits) and row ids cited in prose. The first cell of a generated
/// table row is the row's own key (CLI-035) and is allowed. Real names such as ISO-8601 and
/// SHA-256 are not row ids.
#[test]
fn user_pages_name_no_internal_id() {
    let root = repo_root();
    let internal = Regex::new(r"\b(?:W|H|F|EG|X)\d+\b|\[P\]|\bO-\d+\b").expect("regex");
    let row_id = Regex::new(r"\b[A-Z]{2,6}-\d{2,4}\b").expect("regex");
    let real = ["ISO-", "SHA-", "UTF-", "HTTP-"];
    let mut pages = Vec::new();
    collect_md(&root.join("docs/pages"), &mut pages);
    pages.push(root.join("docs/differences.md"));
    pages.sort();
    let mut hits = Vec::new();
    for path in pages {
        let text = fs::read_to_string(&path).expect("read page");
        let rel = path.strip_prefix(&root).unwrap_or(&path).display().to_string();
        for (i, line) in text.lines().enumerate() {
            let loc = format!("{rel}:{}", i + 1);
            if internal.is_match(line) {
                hits.push(format!("{loc}: internal id: {line}"));
            }
            let prose = if line.starts_with("| ") {
                line.splitn(3, '|').nth(2).unwrap_or("")
            } else {
                line
            };
            for m in row_id.find_iter(prose) {
                if !real.iter().any(|p| m.as_str().starts_with(p)) {
                    hits.push(format!("{loc}: row id `{}` in prose: {line}", m.as_str()));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "internal ids in the user pages:\n{}",
        hits.join("\n")
    );
}

/// settings.md has a row for every key of the schema table (oxa-config/src/schema.rs).
#[test]
fn settings_page_lists_every_schema_key() {
    let root = repo_root();
    let schema = fs::read_to_string(root.join("oxa-config/src/schema.rs")).expect("read schema.rs");
    let key = Regex::new(r#"key\(\s*"([^"]+)""#).expect("regex");
    let mut keys: Vec<String> = key.captures_iter(&schema).map(|c| c[1].to_string()).collect();
    keys.sort();
    keys.dedup();
    assert!(
        keys.len() > 50,
        "the schema has {} keys; the regex missed them",
        keys.len()
    );
    let page = fs::read_to_string(root.join("docs/pages/settings.md")).expect("read settings.md");
    let missing: Vec<&String> = keys
        .iter()
        .filter(|k| !page.contains(&format!("`{k}`")))
        .collect();
    assert!(missing.is_empty(), "settings.md lacks a row for: {missing:?}");
}

/// cli.md documents `oxa wipe`, and sessions.md says what oxa stores and what wipe removes.
#[test]
fn cli_page_documents_wipe_and_the_stored_files() {
    let root = repo_root();
    let cli = fs::read_to_string(root.join("docs/pages/cli.md")).expect("read cli.md");
    for needle in ["oxa wipe --session", "--all", "--engine"] {
        assert!(cli.contains(needle), "cli.md must mention `{needle}`");
    }
    let sessions = fs::read_to_string(root.join("docs/pages/sessions.md")).expect("read sessions.md");
    assert!(
        sessions.contains("## What oxa stores, and how to remove it"),
        "sessions.md must have the storage and removal section"
    );
}

/// A key with an accessor in oxa-config/src/profile.rs has an effect in the code, so settings.md
/// must not call it reserved or say it has no effect (five such keys were documented that way).
#[test]
fn settings_page_does_not_call_a_working_key_reserved() {
    let root = repo_root();
    let profile = fs::read_to_string(root.join("oxa-config/src/profile.rs")).expect("read profile.rs");
    let literal = Regex::new(r#"\(\s*"(oxa\.[A-Za-z.]+)""#).expect("regex");
    let mut accessors: Vec<String> = literal
        .captures_iter(&profile)
        .map(|c| c[1].to_string())
        .collect();
    accessors.sort();
    accessors.dedup();
    assert!(
        accessors.len() >= 5,
        "the accessor regex found {} keys in profile.rs; it missed them",
        accessors.len()
    );
    let page = fs::read_to_string(root.join("docs/pages/settings.md")).expect("read settings.md");
    let mut hits = Vec::new();
    for line in page.lines().filter(|l| l.starts_with("| `")) {
        let cols: Vec<&str> = line.split('|').map(str::trim).collect();
        if cols.len() < 5 {
            continue;
        }
        let key = cols[1].trim_matches('`');
        let meaning = cols[4].to_lowercase();
        let says_inert = meaning.contains("reserved") || meaning.contains("no effect");
        if says_inert && accessors.iter().any(|a| a == key) {
            hits.push(format!("`{key}` has an accessor in profile.rs: {}", cols[4]));
        }
    }
    assert!(
        hits.is_empty(),
        "settings.md calls a working key reserved or inert:\n{}",
        hits.join("\n")
    );
}
