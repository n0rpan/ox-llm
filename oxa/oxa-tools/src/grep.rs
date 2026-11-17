//! A8: the `grep` tool.
// ported from pi packages/coding-agent/src/core/tools/grep.ts
//!
//! The helpers at the bottom of this file are shared with `find.rs` and `ls.rs` (same owner).
use crate::descriptions::{diet, errors, limits};
use crate::resolve_tool_path;
use oxa_proto::{Access, CancelToken, PathFilter, Tool, ToolCtx, ToolOutput, ToolSpec, Value};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::descriptions;

/// `grep` with pi's arguments (pattern, path, glob, ignoreCase, literal, context, limit). In-process: `ignore` walk honouring .gitignore, `regex` matching. Output `path:line: text` sorted by path then line; limit default 100, a limit below 1 is 1; pi's long-line truncation and limit notice. Identical bytes on two runs.
#[derive(Debug, Clone)]
pub struct GrepTool {
    spec: ToolSpec,
}

impl GrepTool {
    pub fn new() -> GrepTool {
        GrepTool {
            spec: crate::spec_of(&descriptions::builtin_specs::GREP_DEFAULT),
        }
    }
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

/// A parsed `grep` call.
struct Query {
    /// The path as the model wrote it (default `.`).
    path: String,
    re: regex::Regex,
    glob: Option<GlobFilter>,
    context: usize,
    limit: usize,
}

/// A `glob` argument: `!` negates, a pattern with `/` is matched against the path relative to
/// the search root, otherwise against the file name at any depth (as ripgrep does).
struct GlobFilter {
    negate: bool,
    matcher: globset::GlobMatcher,
    has_slash: bool,
}

impl GlobFilter {
    fn accepts(&self, rel: &str) -> bool {
        let candidate = if self.has_slash {
            rel
        } else {
            rel.rsplit('/').next().unwrap_or(rel)
        };
        self.matcher.is_match(candidate) != self.negate
    }
}

fn parse(args: &Value) -> Result<Query, String> {
    let pattern = req_str(args, "pattern")?;
    let path = opt_str(args, "path")?.unwrap_or_else(|| ".".to_string());
    let ignore_case = opt_bool(args, "ignoreCase")?;
    let literal = opt_bool(args, "literal")?;
    let context = opt_count(args, "context", 0, 0)?;
    let limit = opt_count(args, "limit", limits::GREP_LIMIT, limits::MIN_LIMIT)?;
    let source = if literal {
        regex::escape(&pattern)
    } else {
        pattern.clone()
    };
    let re = regex::RegexBuilder::new(&source)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| format!("Invalid pattern: {e}"))?;
    let glob = match opt_str(args, "glob")? {
        None => None,
        Some(text) => {
            let (negate, body) = match text.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, text.as_str()),
            };
            let body = strip_dot_slash(body);
            Some(GlobFilter {
                negate,
                matcher: build_glob(body, false)?,
                has_slash: body.contains('/'),
            })
        }
    };
    Ok(Query {
        path,
        re,
        glob,
        context,
        limit,
    })
}

/// Lines and matches collected while scanning, in output order.
#[derive(Default)]
struct Acc {
    out: Vec<String>,
    matches: usize,
    /// The match limit was reached; only trailing context and the look for one more match follow.
    limit_hit: bool,
    /// Index of the file that reached the limit; trailing context is emitted only inside it.
    limit_file: usize,
    /// Last line number that still receives trailing context after the limit match.
    trail_end: usize,
    /// A match exists beyond the limit (pi's `matchLimitReached`).
    extra: bool,
    lines_cut: bool,
    cancelled: bool,
    /// TOOL-DIET: the path is written once per file, its lines indented under it.
    diet: bool,
    /// The file whose path was written last (diet only).
    last_file: Option<String>,
}

impl Tool for GrepTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    /// `[Access::Read(search root)]`.
    fn access(&self, args: &Value, cx: &ToolCtx) -> Result<Vec<Access>, String> {
        let q = parse(args)?;
        Ok(vec![Access::Read(resolve_tool_path(cx, &q.path))])
    }
    fn run(&self, args: &Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        let q = match parse(args) {
            Ok(q) => q,
            Err(e) => return ToolOutput::error(e),
        };
        let root = resolve_tool_path(cx, &q.path);
        let meta = match std::fs::metadata(&root) {
            Ok(m) => m,
            Err(_) => return ToolOutput::error(errors::path_not_found(&q.path)),
        };
        let is_dir = meta.is_dir();
        // (file to read, text shown before the colon). Sorted by relative path, so the output is
        // sorted by path, and the lines of one file are read in order, so by line too.
        let mut files: Vec<(PathBuf, String)> = if is_dir {
            let mut found: Vec<(String, PathBuf)> = walk(&root, cx.hidden.as_ref())
                .filter_map(|r| r.ok())
                .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                .map(|e| (rel_path(&root, e.path()), e.into_path()))
                .collect();
            found.sort();
            found.into_iter().map(|(rel, p)| (p, rel)).collect()
        } else {
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            vec![(root.clone(), name)]
        };
        if let Some(glob) = &q.glob {
            files.retain(|(_, shown)| glob.accepts(shown));
        }
        let mut acc = Acc {
            limit_file: usize::MAX,
            diet: cx.tune.diet,
            ..Acc::default()
        };
        for (idx, (path, shown)) in files.iter().enumerate() {
            if cx.cancel.is_cancelled() {
                acc.cancelled = true;
            }
            if acc.cancelled {
                break;
            }
            scan(path, shown, idx, &q, &mut acc, cx.cancel);
            if acc.extra {
                break;
            }
        }
        if acc.cancelled {
            return ToolOutput::error(errors::ABORTED);
        }
        if acc.matches == 0 {
            return ToolOutput::ok(errors::GREP_NO_MATCHES);
        }
        let cap = crate::read::output_cap(&cx.tune);
        let (body, byte_cut) = cap_output(&acc.out.join("\n"), crate::read::body_cap(&cx.tune));
        let mut notices = Vec::new();
        if acc.extra {
            notices.push(errors::grep_limit_notice(q.limit));
        }
        if byte_cut {
            notices.push(errors::byte_limit_notice(cap));
            if crate::read::budgeted(&cx.tune) {
                notices.push(diet::BYTE_CUT_HINT.to_string());
            }
        }
        if acc.lines_cut {
            notices.push(errors::grep_lines_truncated_notice());
        }
        ToolOutput::ok(format!("{body}{}", errors::notices_suffix(&notices)))
    }
}

/// Open a file for line reading. `None` when it cannot be opened or is binary (a NUL in its
/// first block, as ripgrep skips it).
fn open_text(path: &Path) -> Option<BufReader<File>> {
    let file = File::open(path).ok()?;
    let mut reader = BufReader::with_capacity(8192, file);
    match reader.fill_buf() {
        Ok(block) if !block.contains(&0) => Some(reader),
        _ => None,
    }
}

/// Scan one file line by line, adding matches and context to `acc`.
fn scan(path: &Path, shown: &str, idx: usize, q: &Query, acc: &mut Acc, cancel: &CancelToken) {
    // Binary files (a NUL in the first block) are skipped, as ripgrep does.
    let Some(mut reader) = open_text(path) else { return };
    // Context lines before a match that have not been printed yet.
    let mut before: VecDeque<(usize, String)> = VecDeque::new();
    let mut last_printed = 0usize;
    let mut after_left = 0usize;
    let mut lineno = 0usize;
    let mut raw: Vec<u8> = Vec::new();
    loop {
        if cancel.is_cancelled() {
            acc.cancelled = true;
            return;
        }
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        lineno += 1;
        if raw.last() == Some(&b'\n') {
            raw.pop();
        }
        if raw.last() == Some(&b'\r') {
            raw.pop();
        }
        let text = String::from_utf8_lossy(&raw);
        let is_match = q.re.is_match(&text);

        if acc.limit_hit {
            // Past the limit: a further match proves the limit cut results off; the lines
            // after the last allowed match (inside its context window) are still shown.
            if is_match {
                acc.extra = true;
                return;
            }
            if acc.limit_file == idx && lineno <= acc.trail_end {
                push_line(acc, shown, lineno, &text, false);
            }
            continue;
        }

        if is_match {
            for (n, t) in before.drain(..) {
                if n > last_printed {
                    push_line(acc, shown, n, &t, false);
                }
            }
            push_line(acc, shown, lineno, &text, true);
            last_printed = lineno;
            after_left = q.context;
            acc.matches += 1;
            if acc.matches >= q.limit {
                acc.limit_hit = true;
                acc.limit_file = idx;
                acc.trail_end = lineno + q.context;
            }
        } else if after_left > 0 {
            push_line(acc, shown, lineno, &text, false);
            last_printed = lineno;
            after_left -= 1;
        } else if q.context > 0 {
            before.push_back((lineno, text.into_owned()));
            if before.len() > q.context {
                before.pop_front();
            }
        }
    }
}

/// Append one output line: `path:line: text` for a match, `path-line- text` for context.
fn push_line(acc: &mut Acc, shown: &str, lineno: usize, text: &str, is_match: bool) {
    let max = limits::GREP_MAX_LINE_LENGTH;
    let body = match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((cut, _)) => {
            acc.lines_cut = true;
            format!("{}... [truncated]", &text[..cut])
        }
    };
    if acc.diet {
        if acc.last_file.as_deref() != Some(shown) {
            acc.out.push(shown.to_string());
            acc.last_file = Some(shown.to_string());
        }
        let sep = if is_match { ":" } else { "-" };
        acc.out.push(format!("  {lineno}{sep} {body}"));
        return;
    }
    let line = if is_match {
        format!("{shown}:{lineno}: {body}")
    } else {
        format!("{shown}-{lineno}- {body}")
    };
    acc.out.push(line);
}

// ---- helpers shared by grep, find and ls (same owner, A8) ----

/// The required string argument `key`.
pub(crate) fn req_str(args: &Value, key: &str) -> Result<String, String> {
    opt_str(args, key)?.ok_or_else(|| format!("Invalid arguments: {key} is required"))
}

/// The optional string argument `key`; `null` counts as absent.
pub(crate) fn opt_str(args: &Value, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("Invalid arguments: {key} must be a string")),
    }
}

/// The optional boolean argument `key`, `false` when absent.
pub(crate) fn opt_bool(args: &Value, key: &str) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("Invalid arguments: {key} must be a boolean")),
    }
}

/// The optional numeric argument `key` as a count: floored, at least `min`, at most 2^32 (so the
/// notices' doubled limits cannot overflow). Absent gives `default`.
pub(crate) fn opt_count(args: &Value, key: &str, default: usize, min: usize) -> Result<usize, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => {
            let n = v
                .as_f64()
                .ok_or_else(|| format!("Invalid arguments: {key} must be a number"))?;
            let floored = n.floor();
            let cap = (1u64 << 32) as f64;
            let count = if floored.is_nan() || floored < min as f64 {
                min
            } else if floored > cap {
                cap as usize
            } else {
                floored as usize
            };
            Ok(count)
        }
    }
}

/// Walk `root` with `.gitignore` honoured (also outside a git repository), hidden entries
/// included, `.git` directories skipped, and no reads of the user's global git configuration.
/// #517: entries `hidden` hides (the deny-list below an allowed root) are skipped, and a hidden
/// directory is never entered.
pub(crate) fn walk(root: &Path, hidden: Option<&Arc<dyn PathFilter>>) -> ignore::Walk {
    let hidden = hidden.cloned();
    let given = root.to_path_buf();
    // Resolved once: the walk does not follow links, so the root's real path plus an entry's
    // relative path is the entry's real path.
    let real_root = std::fs::canonicalize(root).unwrap_or_else(|_| given.clone());
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            if e.file_name().to_str() == Some(".git") {
                return false;
            }
            match &hidden {
                Some(h) => {
                    let rel = e.path().strip_prefix(&given).unwrap_or(e.path());
                    !h.hides(&real_root.join(rel))
                }
                None => true,
            }
        })
        .build()
}

/// `p` relative to `root`, components joined with `/`.
pub(crate) fn rel_path(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Keep whole lines of `text` up to `cap` bytes. Returns the kept text and whether anything was
/// cut. A first line longer than `cap` leaves nothing (pi's truncateHead).
pub(crate) fn cap_output(text: &str, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text.to_string(), false);
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let kept = if text.as_bytes().get(end) == Some(&b'\n') {
        &text[..end]
    } else {
        match text[..end].rfind('\n') {
            Some(i) => &text[..i],
            None => "",
        }
    };
    (kept.to_string(), true)
}

/// Compile a glob. `literal_separator`: `*` never crosses `/`, as in ripgrep globs.
pub(crate) fn build_glob(pattern: &str, case_insensitive: bool) -> Result<globset::GlobMatcher, String> {
    globset::GlobBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher())
        .map_err(|e| format!("Invalid glob pattern: {e}"))
}

/// Drop leading `./` segments, which the glob matcher never sees (as pi does for rg and fd).
pub(crate) fn strip_dot_slash(mut s: &str) -> &str {
    while let Some(rest) = s.strip_prefix("./") {
        s = rest;
    }
    s
}
