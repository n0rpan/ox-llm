//! F11 acceptance tests: Docs.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::{parse_script, FakeOpts, FakeServer, TestEnv};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
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

#[test]
fn docs_have_no_merge_conflict_markers() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    for page in ["docs/differences.md", "PARITY.md", "README.md"] {
        let text = std::fs::read_to_string(root.join(page)).unwrap();
        for line in text.lines() {
            assert!(
                !(line.starts_with("<<<<<<< ") || line == "=======" || line.starts_with(">>>>>>> ")),
                "{page}: conflict marker"
            );
        }
    }
}

// ----- shipped CLI and slash inventory --------------------------------------------------------

/// The name of a command signature: `oxa` and the words after it, up to the first placeholder
/// (`<...>` or `[...]`). `oxa wipe --session <id> | --all` -> `oxa wipe --session`.
fn signature_name(cell: &str) -> String {
    let mut keep = Vec::new();
    for word in cell.split_whitespace() {
        if word.starts_with('<') || word.starts_with('[') {
            break;
        }
        keep.push(word.trim_end_matches(','));
    }
    keep.join(" ")
}

/// The long flag of an option cell, when the cell begins with one: `--print, -p` -> `--print`.
fn option_name(cell: &str) -> Option<String> {
    let first = cell.split_whitespace().next()?.trim_end_matches(',');
    if first.starts_with('-') && first.len() > 1 {
        Some(first.to_string())
    } else {
        None
    }
}

/// The whole flags of a documentation cell, split on whitespace and commas: `--print, -p` ->
/// `--print`, `-p`. A flag is a token of its own; `-p` is not documented by `--print`, which
/// merely contains those characters.
fn option_tokens(cell: &str) -> BTreeSet<String> {
    cell.split(|c: char| c.is_whitespace() || c == ',')
        .map(str::trim)
        .filter(|token| token.starts_with('-') && token.len() > 1)
        .map(str::to_string)
        .collect()
}

/// The short aliases of an `OPTIONS` cell that the documentation tokens of its row are missing.
/// Compared as whole tokens, so dropping `-p` from the `--print` row is caught.
fn missing_aliases(cell: &str, documented: &BTreeSet<String>) -> Vec<String> {
    cell.split_whitespace()
        .skip(1)
        .map(|word| word.trim_end_matches(','))
        .filter(|word| word.starts_with('-') && *word != "--")
        .filter(|alias| !documented.contains(*alias))
        .map(str::to_string)
        .collect()
}

/// The first cell of every table row of `text`, with its 1-based line number: backticks stripped
/// and `\|` kept as a literal `|`, so a cell is not cut at an escaped pipe.
fn first_cells(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let Some(rest) = line.trim_start().strip_prefix('|') else {
            continue;
        };
        let mut cell = String::new();
        let mut escaped = false;
        for c in rest.chars() {
            if escaped {
                cell.push(c);
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '|' => break,
                c => cell.push(c),
            }
        }
        out.push((i + 1, cell.replace('`', "").trim().to_string()));
    }
    out
}

/// The slash names of a table cell: `/quit`, `/exit` -> `quit`, `exit`. A `/` inside a word (a
/// path, a key binding) names nothing.
fn slash_names(cell: &str) -> Vec<String> {
    let re = Regex::new(r"/([a-z][a-z-]*)").expect("regex");
    let mut out = Vec::new();
    for m in re.captures_iter(cell) {
        let at = m.get(0).expect("match").start();
        let before = cell[..at].chars().next_back();
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '/' || c == '-') {
            continue;
        }
        out.push(m[1].to_string());
    }
    out
}

/// Every subcommand of the `COMMANDS` table, every option of the `OPTIONS` table and every
/// built-in slash command has a row in the user pages. Rows are matched by name only (the words
/// of the signature before its first placeholder, or the long flag), so the descriptions, the
/// order and the column layout stay free. Short aliases are matched inside the row of their long
/// flag. The three slash commands oxa answers with an `unsupported_in_oxa` notice (`login`,
/// `logout`, `share`) are named in prose instead of a row, and skill, template and extension
/// commands (such as `/web-search`) are not part of the built-in inventory.
///
/// The Commands and Options tables are checked both ways: every shipped name has a row, and no
/// row names a command or option that oxa does not ship.
#[test]
fn cli_and_slash_tables_list_every_shipped_name() {
    let root = repo_root();
    let args = fs::read_to_string(root.join("oxa-cli/src/args.rs")).expect("read args.rs");
    let commands_src = &args[args.find("const COMMANDS").expect("COMMANDS table")
        ..args.find("const OPTIONS").expect("OPTIONS table")];
    let options_src = &args[args.find("const OPTIONS").expect("OPTIONS table")
        ..args.find("fn usage(").expect("end of the OPTIONS table")];
    let command_names: Vec<String> = Regex::new(r#""(oxa [^"]*)""#)
        .expect("regex")
        .captures_iter(commands_src)
        .map(|c| signature_name(&c[1]))
        .collect();
    assert!(
        command_names.len() >= 12,
        "the COMMANDS regex found {} entries; it missed them",
        command_names.len()
    );
    let option_cells: Vec<String> = Regex::new(r#"\(\s*"([^"]*)"\s*,"#)
        .expect("regex")
        .captures_iter(options_src)
        .map(|c| c[1].to_string())
        .collect();
    assert!(
        option_cells.len() >= 40,
        "the OPTIONS regex found {} entries; it missed them",
        option_cells.len()
    );

    let cli = fs::read_to_string(root.join("docs/pages/cli.md")).expect("read cli.md");
    let rows = first_cells(&cli);
    let documented: BTreeSet<String> = rows
        .iter()
        .filter(|(_, cell)| cell.starts_with("oxa "))
        .map(|(_, cell)| signature_name(cell))
        .collect();
    let missing: Vec<&String> = command_names
        .iter()
        .filter(|name| !documented.contains(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/pages/cli.md has no Commands table row for: {missing:?}"
    );
    // The other direction: a row that names a command oxa no longer ships is stale, not coverage.
    let shipped: BTreeSet<&String> = command_names.iter().collect();
    let stale: Vec<&String> = documented
        .iter()
        .filter(|name| !shipped.contains(name))
        .collect();
    assert!(
        stale.is_empty(),
        "docs/pages/cli.md documents commands that oxa does not ship: {stale:?}"
    );

    let mut documented_flags: BTreeMap<String, (usize, BTreeSet<String>)> = BTreeMap::new();
    for (line, cell) in &rows {
        let Some(flag) = option_name(cell) else {
            continue;
        };
        // A table's `|---|---|` separator row begins with dashes but names no option. `--` is a
        // shipped option and is two dashes, so only longer runs are separators.
        if flag.len() > 2 && flag.bytes().all(|byte| byte == b'-') {
            continue;
        }
        documented_flags
            .entry(flag)
            .or_insert_with(|| (*line, BTreeSet::new()))
            .1
            .extend(option_tokens(cell));
    }
    let mut missing = Vec::new();
    let mut alias_gaps = Vec::new();
    for cell in &option_cells {
        let Some(flag) = option_name(cell) else {
            panic!("an OPTIONS entry does not begin with a flag: {cell:?}");
        };
        match documented_flags.get(&flag) {
            None => missing.push(flag),
            Some((line, documented)) => {
                for alias in missing_aliases(cell, documented) {
                    alias_gaps.push(format!("docs/pages/cli.md:{line}: {flag} -> {alias}"));
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "docs/pages/cli.md has no option table row for: {missing:?}"
    );
    assert!(
        alias_gaps.is_empty(),
        "an option row does not show the alias of its long flag:\n{}",
        alias_gaps.join("\n")
    );
    let shipped_flags: BTreeSet<String> = option_cells
        .iter()
        .filter_map(|cell| option_name(cell))
        .collect();
    let stale: Vec<(&String, &usize)> = documented_flags
        .iter()
        .filter(|(flag, _)| !shipped_flags.contains(*flag))
        .map(|(flag, (line, _))| (flag, line))
        .collect();
    assert!(
        stale.is_empty(),
        "docs/pages/cli.md documents options that oxa does not ship (flag, line): {stale:?}"
    );

    let usage = fs::read_to_string(root.join("docs/pages/usage.md")).expect("read usage.md");
    let documented: BTreeSet<String> = first_cells(&usage)
        .iter()
        .flat_map(|(_, cell)| slash_names(cell))
        .collect();
    let slash = fs::read_to_string(root.join("oxa-interactive/src/slash.rs")).expect("read slash.rs");
    let start = slash
        .find("pub const INTERACTIVE_COMMANDS")
        .expect("INTERACTIVE_COMMANDS");
    let block = &slash[start..start + slash[start..].find("];").expect("end of the list")];
    let builtins: Vec<String> = Regex::new(r#""([a-z][a-z-]*)""#)
        .expect("regex")
        .captures_iter(block)
        .map(|c| c[1].to_string())
        .collect();
    assert!(
        builtins.len() >= 30,
        "the slash regex found {} built-ins; it missed them",
        builtins.len()
    );
    // Built-in commands whose answer is the fixed `unsupported_in_oxa` notice.
    const OMITTED: [&str; 3] = ["login", "logout", "share"];
    let missing: Vec<&String> = builtins
        .iter()
        .filter(|name| !documented.contains(*name) && !OMITTED.contains(&name.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/pages/usage.md has no slash table row for: {missing:?}"
    );
    for name in OMITTED {
        assert!(
            usage.contains(&format!("/{name}")),
            "usage.md must say what /{name} does, even though it has no row"
        );
    }
}

/// The alias check compares whole flag tokens: a row that keeps `--print` but drops `-p` is
/// reported, and the substring `-p` inside `--print` does not pass for it.
#[test]
fn cli_option_rows_must_show_their_short_aliases() {
    // Cells arrive with their backticks stripped, as `first_cells` hands them over.
    let complete = option_tokens("--print, -p");
    assert!(complete.contains("--print") && complete.contains("-p"));
    assert_eq!(
        missing_aliases("--print, -p", &complete),
        Vec::<String>::new()
    );

    let dropped = option_tokens("--print");
    assert!(dropped.contains("--print"));
    // `--print` contains the characters `-p`, and still the alias is not documented.
    assert!(!dropped.contains("-p"));
    assert_eq!(
        missing_aliases("--print, -p", &dropped),
        vec!["-p".to_string()],
        "a row that drops its short alias must be reported"
    );

    // The same rule on the published cells: each shipped short alias is reported when its row
    // loses it, and the long flag alone does not cover it.
    for (cell, alias) in [
        ("--print, -p", "-p"),
        ("--resume, -r", "-r"),
        ("--tools, -t <list>", "-t"),
        ("--extension, -e <path>", "-e"),
    ] {
        let tokens = option_tokens(cell);
        assert!(tokens.contains(alias), "{cell} should carry {alias}");
        let without: BTreeSet<String> = tokens
            .iter()
            .filter(|token| *token != alias)
            .cloned()
            .collect();
        assert_eq!(missing_aliases(cell, &without), vec![alias.to_string()]);
    }
}
