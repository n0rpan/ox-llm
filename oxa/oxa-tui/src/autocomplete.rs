//! C13: editor autocompletion: slash commands, `@` fuzzy files, Tab paths. Owner: task C13. Signatures below are frozen; bodies are yours.
// ported from pi packages/tui/src/autocomplete.ts
use std::path::{Path, PathBuf};

use crate::fuzzy::{fuzzy_filter, fuzzy_match};

/// One completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    /// What is inserted.
    pub value: String,
    /// What is shown in the list.
    pub label: String,
    pub description: Option<String>,
}

/// Candidates plus the text they complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestions {
    pub items: Vec<AutocompleteItem>,
    /// What is being matched (`"/"`, `"/mo"`, `"@src/ma"`, `"src/"`).
    pub prefix: String,
}

/// Editor content after a completion was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    /// Byte offset in `lines[cursor_line]`.
    pub cursor_col: usize,
}

/// A source of completions for the editor. Synchronous: an implementation must answer within a
/// few milliseconds (file walks are bounded, see [`FileIndex`]).
pub trait AutocompleteProvider {
    /// Suggestions for the cursor position (`cursor_col` = byte offset in the line), or `None`.
    /// `force` = the user pressed Tab (path completion is offered even without a trigger).
    fn suggestions(
        &mut self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
    ) -> Option<Suggestions>;
    /// Replace `prefix` before the cursor by `item.value` (pi's rules: a completed slash
    /// command gets a trailing space; a directory keeps the list open, i.e. no trailing space;
    /// an `@file` with spaces is quoted).
    fn apply(
        &mut self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> Applied;
}

/// Completions for a command's argument text (see `complete_args`).
pub type ArgCompleter = Box<dyn Fn(&str) -> Vec<AutocompleteItem>>;

/// A slash command offered for completion. Mirrors `oxa_proto::CommandSpec` plus optional
/// argument completions (kept separate so this crate needs no session types).
pub struct SlashCommand {
    /// Without the leading `/`.
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    /// Completions for the argument text typed so far; `None` = no argument completion.
    pub complete_args: Option<ArgCompleter>,
}

/// A bounded, `.gitignore`-aware listing of files under a root for `@` completion.
///
/// Built by walking `root` (skipping `.git`, honouring `.gitignore` files found on the way with
/// plain glob semantics: this crate has no `ignore` dependency, so nested negations may be
/// approximated), stopping after `max_files` entries. Paths are relative to `root`, `/`
/// separated, directories end in `/`, sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIndex {
    pub root: PathBuf,
    pub paths: Vec<String>,
    /// `true` when the walk stopped at `max_files`.
    pub truncated: bool,
}

/// One line of a `.gitignore`, anchored to the directory that holds the file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IgnoreRule {
    /// Directory of the `.gitignore`, relative to the root (`""` for the root itself).
    base: String,
    pattern: String,
    negated: bool,
    dir_only: bool,
    /// The pattern contained a `/` (before a trailing one was removed): match the whole path
    /// below `base`, else match the final component at any depth.
    anchored: bool,
}

impl IgnoreRule {
    fn matches(&self, rel: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let sub = if self.base.is_empty() {
            rel
        } else {
            match rel
                .strip_prefix(self.base.as_str())
                .and_then(|s| s.strip_prefix('/'))
            {
                Some(s) => s,
                None => return false,
            }
        };
        if self.anchored {
            glob_match(self.pattern.as_bytes(), sub.as_bytes())
        } else {
            glob_match(self.pattern.as_bytes(), basename(sub).as_bytes())
        }
    }
}

fn parse_gitignore(base: &str, content: &str) -> Vec<IgnoreRule> {
    let mut rules = Vec::new();
    for raw in content.lines() {
        let line = raw.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negated, mut pat) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let dir_only = pat.ends_with('/');
        if dir_only {
            pat = pat.trim_end_matches('/');
        }
        let anchored = pat.contains('/');
        let pat = pat.trim_start_matches('/');
        if pat.is_empty() {
            continue;
        }
        rules.push(IgnoreRule {
            base: base.to_string(),
            pattern: pat.to_string(),
            negated,
            dir_only,
            anchored,
        });
    }
    rules
}

/// Git semantics: the last matching rule decides; `!` re-includes.
fn is_ignored(rules: &[IgnoreRule], rel: &str, is_dir: bool) -> bool {
    let mut ignored = false;
    for r in rules {
        if r.matches(rel, is_dir) {
            ignored = !r.negated;
        }
    }
    ignored
}

fn char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >= 0xF0 {
        4
    } else if b >= 0xE0 {
        3
    } else if b >= 0xC0 {
        2
    } else {
        1
    }
}

/// The first character of `t` and its byte length.
fn first_char(t: &[u8]) -> Option<(char, usize)> {
    let &b = t.first()?;
    let n = char_len(b).min(t.len());
    let c = std::str::from_utf8(&t[..n]).ok()?.chars().next()?;
    Some((c, n))
}

/// Index of the `]` closing the class that starts at `p[0] == b'['`, if any.
fn class_end(p: &[u8]) -> Option<usize> {
    let mut j = 1;
    if matches!(p.get(1), Some(b'!') | Some(b'^')) {
        j = 2;
    }
    if p.get(j) == Some(&b']') {
        j += 1;
    }
    p[j.min(p.len())..]
        .iter()
        .position(|&b| b == b']')
        .map(|off| j + off)
}

/// `body` is the text between `[` and `]`.
fn match_class(body: &[u8], c: char) -> bool {
    let (negate, body) = match body.first() {
        Some(b'!') | Some(b'^') => (true, &body[1..]),
        _ => (false, body),
    };
    let chars: Vec<char> = std::str::from_utf8(body)
        .map(|s| s.chars().collect())
        .unwrap_or_default();
    let mut matched = false;
    let mut k = 0;
    while k < chars.len() {
        if k + 2 < chars.len() && chars[k + 1] == '-' {
            if chars[k] <= c && c <= chars[k + 2] {
                matched = true;
            }
            k += 3;
        } else {
            if chars[k] == c {
                matched = true;
            }
            k += 1;
        }
    }
    matched != negate
}

/// Gitignore glob: `*` and `?` do not cross `/`, `**` does (`**/` also matches zero
/// directories), `[...]` classes, `\x` escapes.
fn glob_match(p: &[u8], t: &[u8]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    if p.starts_with(b"**") {
        let rest = &p[2..];
        if rest.first() == Some(&b'/') && glob_match(&rest[1..], t) {
            return true;
        }
        let mut i = 0;
        loop {
            if glob_match(rest, &t[i..]) {
                return true;
            }
            if i >= t.len() {
                return false;
            }
            i = (i + char_len(t[i])).min(t.len());
        }
    }
    match p[0] {
        b'*' => {
            let mut i = 0;
            loop {
                if glob_match(&p[1..], &t[i..]) {
                    return true;
                }
                if i >= t.len() || t[i] == b'/' {
                    return false;
                }
                i = (i + char_len(t[i])).min(t.len());
            }
        }
        b'?' => match first_char(t) {
            Some((c, n)) if c != '/' => glob_match(&p[1..], &t[n..]),
            _ => false,
        },
        b'[' => match class_end(p) {
            Some(close) => match first_char(t) {
                Some((c, n)) if c != '/' && match_class(&p[1..close], c) => {
                    glob_match(&p[close + 1..], &t[n..])
                }
                _ => false,
            },
            None => t.first() == Some(&b'[') && glob_match(&p[1..], &t[1..]),
        },
        b'\\' if p.len() >= 2 => t.first() == Some(&p[1]) && glob_match(&p[2..], &t[1..]),
        c => t.first() == Some(&c) && glob_match(&p[1..], &t[1..]),
    }
}

/// Recursive walk in sorted order. Returns `false` when the walk must stop (cap reached).
fn walk_dir(
    root: &Path,
    rel_dir: &str,
    rules: &mut Vec<IgnoreRule>,
    max_files: usize,
    out: &mut Vec<String>,
    truncated: &mut bool,
) -> bool {
    let dir = if rel_dir.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel_dir)
    };
    let mark = rules.len();
    if let Ok(content) = std::fs::read_to_string(dir.join(".gitignore")) {
        rules.extend(parse_gitignore(rel_dir, &content));
    }

    let mut entries: Vec<(String, bool, bool)> = Vec::new(); // (name, is_dir, is_symlink)
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(ft) = e.file_type() else { continue };
            let is_link = ft.is_symlink();
            let is_dir = if is_link {
                std::fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false)
            } else {
                ft.is_dir()
            };
            entries.push((name, is_dir, is_link));
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut keep_going = true;
    for (name, is_dir, is_link) in entries {
        if name == ".git" {
            continue;
        }
        let rel = if rel_dir.is_empty() {
            name
        } else {
            format!("{rel_dir}/{name}")
        };
        if is_ignored(rules, &rel, is_dir) {
            continue;
        }
        if out.len() >= max_files {
            *truncated = true;
            keep_going = false;
            break;
        }
        out.push(if is_dir { format!("{rel}/") } else { rel.clone() });
        // Symlinked directories are listed but not entered (no cycles).
        if is_dir && !is_link && !walk_dir(root, &rel, rules, max_files, out, truncated) {
            keep_going = false;
            break;
        }
    }
    rules.truncate(mark);
    keep_going
}

impl FileIndex {
    pub fn build(root: &Path, max_files: usize) -> FileIndex {
        let mut paths = Vec::new();
        let mut truncated = false;
        let mut rules = Vec::new();
        walk_dir(root, "", &mut rules, max_files, &mut paths, &mut truncated);
        paths.sort();
        FileIndex {
            root: root.to_path_buf(),
            paths,
            truncated,
        }
    }
    /// An index over a fixed list (tests).
    pub fn from_paths(root: &Path, paths: Vec<String>) -> FileIndex {
        FileIndex {
            root: root.to_path_buf(),
            paths,
            truncated: false,
        }
    }
}

/// Extension command-name candidates (see `CombinedProvider::set_extra`).
type ExtraCompleter = Box<dyn Fn(&str) -> Vec<String>>;

/// pi's combined provider: `/` commands at the start of the text, `@` fuzzy file search at a
/// token boundary, and path completion on Tab.
///
/// Acceptance (plan C13): `/` commands, `@` fuzzy files (.gitignore), Tab paths;
/// `autocompleteMaxVisible` (the list height is the caller's: this type returns at most
/// `max_items` candidates, default 100).
pub struct CombinedProvider {
    _owner_defines_fields: (),
    commands: Vec<SlashCommand>,
    files: FileIndex,
    cwd: PathBuf,
    home: PathBuf,
    max_items: usize,
    extra: Option<ExtraCompleter>,
}

impl CombinedProvider {
    /// `cwd` is the base for relative paths and `~` expands to `home`.
    pub fn new(commands: Vec<SlashCommand>, files: FileIndex, cwd: &Path, home: &Path) -> CombinedProvider {
        CombinedProvider {
            _owner_defines_fields: (),
            commands,
            files,
            cwd: cwd.to_path_buf(),
            home: home.to_path_buf(),
            max_items: 100,
            extra: None,
        }
    }
    pub fn set_commands(&mut self, commands: Vec<SlashCommand>) {
        self.commands = commands;
    }
    pub fn set_files(&mut self, files: FileIndex) {
        self.files = files;
    }
    pub fn set_max_items(&mut self, max_items: usize) {
        self.max_items = max_items;
    }
    /// Extra candidates from extensions (`Hooks::complete`), merged after the built-in ones.
    /// Called with the command-name prefix (the text after `/`, before any space) and
    /// returns candidate command names; each becomes an item with `label == value`.
    pub fn set_extra(&mut self, extra: ExtraCompleter) {
        self.extra = Some(extra);
    }

    /// Slash-command names matching `prefix`, with pi's `hint — description` text.
    fn command_items(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let names: Vec<String> = self.commands.iter().map(|c| c.name.clone()).collect();
        let mut items: Vec<AutocompleteItem> = fuzzy_filter(&names, prefix)
            .into_iter()
            .map(|i| {
                let cmd = &self.commands[i];
                AutocompleteItem {
                    value: cmd.name.clone(),
                    label: cmd.name.clone(),
                    description: command_description(cmd),
                }
            })
            .collect();
        if let Some(extra) = &self.extra {
            for s in extra(prefix) {
                if !items.iter().any(|it| it.value == s) {
                    items.push(AutocompleteItem {
                        value: s.clone(),
                        label: s,
                        description: None,
                    });
                }
            }
        }
        items.truncate(self.max_items);
        items
    }

    /// `@` completion over the file index. `raw` is the text after `@` (quotes removed).
    /// A query with a `/` searches below the typed directory and keeps that base in the
    /// inserted text. Ranking as pi: basename exact > basename prefix > basename substring >
    /// path substring > fuzzy path match; directories get a bonus; ties by depth, length, name.
    fn fuzzy_file_items(&self, raw: &str, is_quoted: bool) -> Vec<AutocompleteItem> {
        let (display_base, query) = match raw.rfind('/') {
            Some(i) => (&raw[..=i], &raw[i + 1..]),
            None => ("", raw),
        };
        // The index covers the root only; absolute and home-relative bases are not indexed.
        if display_base.starts_with('/') || display_base.starts_with('~') {
            return Vec::new();
        }
        let search_base = display_base.strip_prefix("./").unwrap_or(display_base);
        let q_lower = query.to_lowercase();
        let show_hidden = hidden_allowed(query);

        // (score, depth, relative path without trailing slash, is_dir)
        let mut scored: Vec<(f64, usize, String, bool)> = Vec::new();
        for entry in &self.files.paths {
            let Some(rest) = entry.strip_prefix(search_base) else {
                continue;
            };
            let is_dir = rest.ends_with('/');
            let rel = rest.strip_suffix('/').unwrap_or(rest);
            if rel.is_empty() || (!show_hidden && is_hidden_rel(rel)) {
                continue;
            }
            let score = entry_score(rel, query, &q_lower, is_dir);
            if score > 0.0 {
                let depth = rel.matches('/').count();
                scored.push((score, depth, rel.to_string(), is_dir));
            }
        }
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
                .then(a.2.len().cmp(&b.2.len()))
                .then(a.2.cmp(&b.2))
        });

        let scoped = !display_base.is_empty();
        scored
            .into_iter()
            .take(self.max_items)
            .map(|(_, _, rel, is_dir)| {
                let display = if scoped {
                    format!("{display_base}{rel}")
                } else {
                    rel.clone()
                };
                let completion = if is_dir {
                    format!("{display}/")
                } else {
                    display.clone()
                };
                AutocompleteItem {
                    value: build_completion_value(&completion, true, is_quoted),
                    label: format!("{}{}", basename(&rel), if is_dir { "/" } else { "" }),
                    description: Some(display),
                }
            })
            .collect()
    }

    /// Tab / path completion relative to `cwd`, `~` and absolute prefixes (pi's
    /// `getFileSuggestions`). Directories sort first and carry a trailing `/`.
    fn path_items(&self, prefix: &str) -> Vec<AutocompleteItem> {
        let (raw, is_at, is_quoted) = parse_path_prefix(prefix);
        let expanded = if raw.starts_with('~') {
            expand_home(raw, &self.home)
        } else {
            raw.to_string()
        };
        let is_root = matches!(raw, "" | "./" | "../" | "~" | "~/" | "/");
        let absolute_like = raw.starts_with('~') || expanded.starts_with('/');
        let resolve = |d: &str| -> PathBuf {
            if absolute_like {
                PathBuf::from(d)
            } else {
                self.cwd.join(d)
            }
        };

        let (dir_part, search_prefix): (String, String) = if is_root || raw.ends_with('/') {
            (expanded.clone(), String::new())
        } else {
            (dirname(&expanded).to_string(), basename(&expanded).to_string())
        };
        let search_dir = resolve(&dir_part);

        let Ok(read) = std::fs::read_dir(&search_dir) else {
            return Vec::new();
        };
        let mut entries: Vec<(String, bool)> = Vec::new();
        for e in read.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.to_lowercase().starts_with(&search_prefix.to_lowercase()) {
                continue;
            }
            if name.starts_with('.') && !hidden_allowed(&search_prefix) {
                continue;
            }
            let is_dir = match e.file_type() {
                Ok(ft) if ft.is_symlink() => std::fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false),
                Ok(ft) => ft.is_dir(),
                Err(_) => false,
            };
            entries.push((name, is_dir));
        }

        let mut items: Vec<AutocompleteItem> = entries
            .into_iter()
            .map(|(name, is_dir)| {
                let rel = if raw.ends_with('/') {
                    format!("{raw}{name}")
                } else if raw.contains('/') {
                    if let Some(home_rel) = raw.strip_prefix("~/") {
                        let dir = dirname(home_rel);
                        if dir == "." {
                            format!("~/{name}")
                        } else {
                            format!("~/{dir}/{name}")
                        }
                    } else if raw.starts_with('/') {
                        let dir = dirname(raw);
                        if dir == "/" {
                            format!("/{name}")
                        } else {
                            format!("{dir}/{name}")
                        }
                    } else {
                        let dir = dirname(raw);
                        let joined = if dir == "." {
                            name.clone()
                        } else {
                            format!("{dir}/{name}")
                        };
                        if raw.starts_with("./") && !joined.starts_with("./") {
                            format!("./{joined}")
                        } else {
                            joined
                        }
                    }
                } else if raw.starts_with('~') {
                    format!("~/{name}")
                } else {
                    name.clone()
                };
                let path_value = if is_dir { format!("{rel}/") } else { rel };
                AutocompleteItem {
                    value: build_completion_value(&path_value, is_at, is_quoted),
                    label: format!("{name}{}", if is_dir { "/" } else { "" }),
                    description: None,
                }
            })
            .collect();

        items.sort_by(|a, b| {
            let (ad, bd) = (a.label.ends_with('/'), b.label.ends_with('/'));
            bd.cmp(&ad)
                .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
                .then_with(|| a.label.cmp(&b.label))
        });
        items.truncate(self.max_items);
        items
    }
}

impl AutocompleteProvider for CombinedProvider {
    fn suggestions(
        &mut self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
    ) -> Option<Suggestions> {
        let line = lines.get(cursor_line).map(String::as_str).unwrap_or("");
        let col = clamp_boundary(line, cursor_col);
        let text = &line[..col];

        // `@` file search wins everywhere it is triggered.
        if let Some(at_prefix) = extract_at_prefix(text) {
            let (raw, _, is_quoted) = parse_path_prefix(at_prefix);
            let items = self.fuzzy_file_items(raw, is_quoted);
            if items.is_empty() {
                return None;
            }
            return Some(Suggestions {
                items,
                prefix: at_prefix.to_string(),
            });
        }

        let command_text = text.trim_start();
        if !force && command_text.starts_with('/') {
            return match command_text.find(' ') {
                None => {
                    let prefix = &command_text[1..];
                    let items = self.command_items(prefix);
                    if items.is_empty() {
                        None
                    } else {
                        Some(Suggestions {
                            items,
                            prefix: command_text.to_string(),
                        })
                    }
                }
                Some(space) => {
                    let name = &command_text[1..space];
                    let argument_text = &command_text[space + 1..];
                    let command = self.commands.iter().find(|c| c.name == name)?;
                    let complete = command.complete_args.as_ref()?;
                    let mut items = complete(argument_text);
                    items.truncate(self.max_items);
                    if items.is_empty() {
                        None
                    } else {
                        Some(Suggestions {
                            items,
                            prefix: argument_text.to_string(),
                        })
                    }
                }
            };
        }

        let path_prefix = extract_path_prefix(text, force)?;
        let items = self.path_items(&path_prefix);
        if items.is_empty() {
            return None;
        }
        Some(Suggestions {
            items,
            prefix: path_prefix,
        })
    }

    fn apply(
        &mut self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> Applied {
        let line = lines.get(cursor_line).map(String::as_str).unwrap_or("");
        let col = clamp_boundary(line, cursor_col);
        let start = clamp_boundary(line, col.saturating_sub(prefix.len()));
        let before = &line[..start];
        let after = &line[col..];

        let is_quoted = prefix.starts_with('"') || prefix.starts_with("@\"");
        let adjusted_after = if is_quoted && item.value.ends_with('"') && after.starts_with('"') {
            &after[1..]
        } else {
            after
        };
        let is_dir = item.label.ends_with('/');
        let trailing_quote_dir_adjust = |len: usize| -> usize {
            if is_dir && item.value.ends_with('"') {
                len - 1
            } else {
                len
            }
        };

        let (new_line, cursor_offset) =
            if prefix.starts_with('/') && before.trim().is_empty() && !prefix[1..].contains('/') {
                // A command name: "/" + name + a trailing space; the cursor goes after the space.
                let new_line = format!("{before}/{} {adjusted_after}", item.value);
                (new_line, before.len() + item.value.len() + 2)
            } else if prefix.starts_with('@') {
                // An `@file`: directories keep the list open (no trailing space).
                let suffix = if is_dir { "" } else { " " };
                let new_line = format!("{before}{}{suffix}{adjusted_after}", item.value);
                let off = before.len() + trailing_quote_dir_adjust(item.value.len()) + suffix.len();
                (new_line, off)
            } else {
                // A path or a command argument.
                let new_line = format!("{before}{}{adjusted_after}", item.value);
                let off = before.len() + trailing_quote_dir_adjust(item.value.len());
                (new_line, off)
            };

        let mut new_lines = lines.to_vec();
        while new_lines.len() <= cursor_line {
            new_lines.push(String::new());
        }
        new_lines[cursor_line] = new_line;
        Applied {
            lines: new_lines,
            cursor_line,
            cursor_col: cursor_offset,
        }
    }
}

/// Fuzzy score of one `@` candidate (lower is better, matching pi's `fuzzyFilter` sort after
/// `scoreEntry`'s tiers, higher-is-better here). `0` means "not a candidate".
fn entry_score(rel: &str, query: &str, q_lower: &str, is_dir: bool) -> f64 {
    if query.is_empty() {
        return 1.0;
    }
    let name = basename(rel).to_lowercase();
    let mut score = if name == q_lower {
        100.0
    } else if name.starts_with(q_lower) {
        80.0
    } else if name.contains(q_lower) {
        50.0
    } else if rel.to_lowercase().contains(q_lower) {
        30.0
    } else if fuzzy_match(query, rel).matches {
        10.0
    } else {
        0.0
    };
    if is_dir && score > 0.0 {
        score += 10.0;
    }
    score
}

fn command_description(cmd: &SlashCommand) -> Option<String> {
    let desc = cmd.description.clone().unwrap_or_default();
    let hint = cmd.argument_hint.clone().filter(|h| !h.is_empty());
    match (hint, desc.is_empty()) {
        (Some(h), false) => Some(format!("{h} — {desc}")),
        (Some(h), true) => Some(h),
        (None, false) => Some(desc),
        (None, true) => None,
    }
}

const PATH_DELIMITERS: &[char] = &[' ', '\t', '"', '\'', '='];
const PATH_WRAPPERS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('<', '>'), ('`', '`')];

/// pi's `autocompleteSeparatorRegex`: whitespace or CJK punctuation.
fn is_separator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '，' | '．'
                | '：'
                | '；'
                | '！'
                | '？'
                | '（'
                | '）'
                | '［'
                | '］'
                | '｛'
                | '｝'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '…'
                | '—'
                | '、'
                | '。'
                | '「'
                | '」'
                | '【'
                | '】'
                | '《'
                | '》'
                | '〈'
                | '〉'
        )
}

/// The closing counterpart of an opening wrapper such as `(`.
fn wrapper_closer(c: char) -> Option<char> {
    PATH_WRAPPERS
        .iter()
        .find(|(open, _)| *open == c)
        .map(|(_, close)| *close)
}

/// Move `idx` down to the nearest char boundary within `s` (and to `s.len()` at most).
fn clamp_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn prev_char(s: &str, idx: usize) -> Option<char> {
    s[..idx].chars().next_back()
}

/// pi's `isTokenStart`: `index` begins a token (start of text, after a delimiter, or after a
/// separator), skipping opening wrappers in front of it.
fn is_token_start(text: &str, index: usize) -> bool {
    let mut start = index;
    while let Some(c) = prev_char(text, start) {
        if wrapper_closer(c).is_some() {
            start -= c.len_utf8();
        } else {
            break;
        }
    }
    match prev_char(text, start) {
        None => true,
        Some(c) => PATH_DELIMITERS.contains(&c) || is_separator(c),
    }
}

/// Byte index of the last unclosed `"`, if the text ends inside a quote.
fn find_unclosed_quote_start(text: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut quote_start = 0;
    for (i, c) in text.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
            if in_quotes {
                quote_start = i;
            }
        }
    }
    if in_quotes {
        Some(quote_start)
    } else {
        None
    }
}

/// `@"..."` or `"..."` still open at the end of the text, when it starts a token.
fn extract_quoted_prefix(text: &str) -> Option<&str> {
    let qs = find_unclosed_quote_start(text)?;
    if qs > 0 && text.as_bytes()[qs - 1] == b'@' {
        if !is_token_start(text, qs - 1) {
            return None;
        }
        return Some(&text[qs - 1..]);
    }
    if !is_token_start(text, qs) {
        return None;
    }
    Some(&text[qs..])
}

/// The text after the last delimiter or separator (the whole text if there is none).
fn last_token(text: &str) -> &str {
    let mut token_start = 0;
    for (i, c) in text.char_indices() {
        if PATH_DELIMITERS.contains(&c) || is_separator(c) {
            token_start = i + c.len_utf8();
        }
    }
    &text[token_start..]
}

/// Drop opening wrappers before a path, e.g. `(~/Dev` -> `~/Dev`. A wrapper is kept when the
/// token also contains its closer (`app/[slug]/pa`).
fn strip_leading_wrappers(token: &str) -> &str {
    let mut r = token;
    while let Some(c) = r.chars().next() {
        match wrapper_closer(c) {
            Some(closer) if !r[c.len_utf8()..].contains(closer) => r = &r[c.len_utf8()..],
            _ => break,
        }
    }
    r
}

/// `@file` token (quoted or not) before the cursor.
fn extract_at_prefix(text: &str) -> Option<&str> {
    if let Some(q) = extract_quoted_prefix(text) {
        if q.starts_with("@\"") {
            return Some(q);
        }
    }
    let token = strip_leading_wrappers(last_token(text));
    if token.starts_with('@') {
        Some(token)
    } else {
        None
    }
}

/// `true` when `raw` (a path prefix without `@` / quotes) is an explicit path: it starts with
/// `./`, `../`, `~/` or `/`.
fn is_explicit_path(raw: &str) -> bool {
    raw.starts_with("./") || raw.starts_with("../") || raw.starts_with("~/") || raw.starts_with('/')
}

/// The path-like prefix for Tab (forced) or an explicit path token (`./`, `../`, `~/`, `/`),
/// or `None`. Without Tab, prose never opens the list: not a quote (`what's `, `it's "fine`),
/// not a trailing space, not a word with a `/` in it (`and/or`).
fn extract_path_prefix(text: &str, force: bool) -> Option<String> {
    if let Some(q) = extract_quoted_prefix(text) {
        if force || is_explicit_path(parse_path_prefix(q).0) {
            return Some(q.to_string());
        }
        return None;
    }
    let token = strip_leading_wrappers(last_token(text));
    if force || is_explicit_path(token) {
        return Some(token.to_string());
    }
    None
}

/// A hidden entry (a path component that starts with `.`) is offered only when the typed name
/// starts with `.` (`@.c`, `./.git`, Tab on `.ca`).
fn hidden_allowed(query: &str) -> bool {
    query.starts_with('.')
}

fn is_hidden_rel(rel: &str) -> bool {
    rel.split('/')
        .any(|c| c.starts_with('.') && c != "." && c != "..")
}

/// `(raw, is_at, is_quoted)` for a prefix such as `@"src/ma`, `"src/ma`, `@src/ma`, `src/ma`.
fn parse_path_prefix(prefix: &str) -> (&str, bool, bool) {
    if let Some(r) = prefix.strip_prefix("@\"") {
        (r, true, true)
    } else if let Some(r) = prefix.strip_prefix('"') {
        (r, false, true)
    } else if let Some(r) = prefix.strip_prefix('@') {
        (r, true, false)
    } else {
        (prefix, false, false)
    }
}

/// The inserted text for a path: quoted when the typed prefix was quoted or the path contains
/// whitespace or CJK punctuation; an `@` is kept in front for `@` completions.
fn build_completion_value(path: &str, is_at: bool, is_quoted: bool) -> String {
    let needs_quotes = is_quoted || path.chars().any(is_separator);
    let at = if is_at { "@" } else { "" };
    if needs_quotes {
        format!("{at}\"{path}\"")
    } else {
        format!("{at}{path}")
    }
}

/// `~/x` and `~` become the home path; a trailing `/` is kept.
fn expand_home(path: &str, home: &Path) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        let mut p = home.join(rest).to_string_lossy().into_owned();
        if path.ends_with('/') && !p.ends_with('/') {
            p.push('/');
        }
        p
    } else if path == "~" {
        home.to_string_lossy().into_owned()
    } else {
        path.to_string()
    }
}

/// Node's `path.dirname` for the separators used here (`/` only).
fn dirname(p: &str) -> &str {
    match p.rfind('/') {
        None => ".",
        Some(0) => "/",
        Some(i) => &p[..i],
    }
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}
