//! B8: prompt templates.
// ported from pi packages/coding-agent/src/core/prompt-templates.ts
use crate::frontmatter::{frontmatter_get, parse_frontmatter};
use crate::skills::{canonical_path, resolve_setting_path, utf16_len};
use crate::{Diagnostic, DiscoverOpts, ResourceSource, Template};
use oxa_config::{Dirs, Settings};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// pi's fallback description: the first non-empty body line, cut to this many UTF-16 units.
const FALLBACK_DESCRIPTION_MAX: usize = 60;

/// Discover templates. Order (first wins on a name clash, the loser is a warning):
/// `opts.extra_templates`, then (unless `opts.no_templates`) `<agent dir>/prompts/*.md`,
/// project `.oxa/prompts/*.md` (trusted), settings `prompts` paths, `package_paths`.
/// The result is sorted by name.
pub fn discover_templates(
    dirs: &Dirs,
    cwd: &Path,
    settings: &Settings,
    opts: &DiscoverOpts,
    package_paths: &[(PathBuf, ResourceSource)],
) -> (Vec<Template>, Vec<Diagnostic>) {
    let mut loader = TemplateLoader::default();
    for path in &opts.extra_templates {
        loader.load_explicit(path, &ResourceSource::Cli, true);
    }
    if !opts.no_templates {
        loader.load_dir(&dirs.agent_dir.join("prompts"), &ResourceSource::User);
        if opts.trusted {
            loader.load_dir(&dirs.project_dir(cwd).join("prompts"), &ResourceSource::Project);
        }
        for entry in settings.list("prompts") {
            if entry.trim_start().starts_with('!') {
                loader.diags.push(Diagnostic::warning(
                    None,
                    format!(
                        "prompts setting entry `{entry}` (exclusion) is not supported yet and was ignored"
                    ),
                ));
                continue;
            }
            let path = resolve_setting_path(&entry, &dirs.home, cwd);
            loader.load_explicit(&path, &ResourceSource::Settings, false);
        }
        for (path, source) in package_paths {
            loader.load_explicit(path, source, false);
        }
    }
    loader.finish()
}

/// Load one template file. The name is the file stem; the description is the frontmatter
/// `description` or the first non-empty body line.
pub fn load_template_file(path: &Path, source: ResourceSource) -> Result<Template, Diagnostic> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| Diagnostic::warning(Some(path), format!("failed to read prompt template file: {e}")))?;
    let (pairs, body) = parse_frontmatter(&content);

    let name = path.file_stem().and_then(OsStr::to_str).unwrap_or("").to_string();
    let description = match frontmatter_get(&pairs, "description") {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => fallback_description(body),
    };
    let argument_hint = frontmatter_get(&pairs, "argument-hint")
        .filter(|h| !h.is_empty())
        .map(str::to_string);

    Ok(Template {
        name,
        description,
        argument_hint,
        body: body.to_string(),
        path: path.to_path_buf(),
        source,
    })
}

/// Split a command's argument text with pi's shell-like quoting: a quote starts quoting only
/// at the beginning of an argument, so apostrophes inside words (`doesn't`) stay text; an
/// unterminated quote is literal text (the CHANGELOG cases are the tests).
pub fn parse_template_args(text: &str) -> Vec<String> {
    // A line-by-line port of pi's `parseCommandArgs`, including its rewind of an unclosed quote.
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut in_quote: Option<char> = None;
    let mut quote_start = 0usize;

    let mut i = 0usize;
    while i < n {
        let c = chars[i];
        match in_quote {
            Some(q) => {
                if c == '\\' && i + 1 < n && chars[i + 1] == q {
                    // `\"` inside `"..."` is a literal quote.
                    current.push(q);
                    i += 1;
                } else if c == q {
                    in_quote = None;
                } else {
                    current.push(c);
                }
            }
            None => {
                if (c == '"' || c == '\'') && !has_token {
                    in_quote = Some(c);
                    quote_start = i;
                    has_token = true;
                } else if c.is_whitespace() {
                    if has_token {
                        args.push(std::mem::take(&mut current));
                        has_token = false;
                    }
                } else {
                    current.push(c);
                    has_token = true;
                }
            }
        }
        if in_quote.is_some() && i == n - 1 {
            // Unclosed quote: the quote character is text, and the rest is parsed normally.
            in_quote = None;
            current = chars[quote_start].to_string();
            i = quote_start;
        }
        i += 1;
    }
    if has_token {
        args.push(current);
    }
    args
}

/// Expand `$1`, `$2`, .., `$@`, `$ARGUMENTS`, `${1:-default}`, `${@:-default}`, `${@:N}` and
/// `${@:N:L}` in `body`. A missing positional argument expands to the empty string. Expansion
/// is single-pass: text inserted from an argument is never expanded again.
pub fn expand_template(body: &str, args: &[String]) -> String {
    let all = args.join(" ");
    let bytes = body.as_bytes();
    let mut out = String::with_capacity(body.len());
    let mut last = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        match match_at(body, i, args, &all) {
            Some((end, replacement)) => {
                out.push_str(&body[last..i]);
                out.push_str(&replacement);
                last = end;
                i = end;
            }
            None => i += 1,
        }
    }
    out.push_str(&body[last..]);
    out
}

/// If `input` is `/<name> [args]` for one of `templates`, the expanded text; else `None`.
/// Expansion happens before the text becomes a message.
pub fn expand_template_command(templates: &[Template], input: &str) -> Option<String> {
    let rest = input.strip_prefix('/')?;
    let name_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
    if name_len == 0 {
        return None;
    }
    let name = &rest[..name_len];
    let args_text = rest[name_len..].trim_start();
    let template = templates.iter().find(|t| t.name == name)?;
    Some(expand_template(&template.body, &parse_template_args(args_text)))
}

// ---- internal helpers ---------------------------------------------------------------------

#[derive(Default)]
struct TemplateLoader {
    found: Vec<Template>,
    diags: Vec<Diagnostic>,
    /// Canonical paths already loaded (symlink aliases).
    seen: HashSet<PathBuf>,
}

impl TemplateLoader {
    /// An explicit path. A directory is scanned (not recursively), a `.md` file is loaded. A
    /// missing path is reported only for command-line paths (`cli`), as in pi.
    fn load_explicit(&mut self, path: &Path, source: &ResourceSource, cli: bool) {
        if !path.exists() {
            if cli {
                self.diags.push(Diagnostic::warning(
                    Some(path),
                    "prompt template path does not exist",
                ));
            }
            return;
        }
        if path.is_dir() {
            self.load_dir(path, source);
        } else if path.is_file() && path.to_string_lossy().ends_with(".md") {
            self.load_file(path, source);
        } else if cli {
            self.diags.push(Diagnostic::warning(
                Some(path),
                "prompt template path is not a markdown file",
            ));
        }
    }

    /// Every `.md` file directly inside `dir`, sorted by path; subdirectories are not visited.
    fn load_dir(&mut self, dir: &Path, source: &ResourceSource) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
        entries.sort();
        for entry in entries {
            let is_md = entry.to_string_lossy().ends_with(".md");
            if is_md && entry.is_file() {
                self.load_file(&entry, source);
            }
        }
    }

    fn load_file(&mut self, path: &Path, source: &ResourceSource) {
        if !self.seen.insert(canonical_path(path)) {
            return;
        }
        match load_template_file(path, source.clone()) {
            Ok(template) => self.found.push(template),
            Err(diag) => self.diags.push(diag),
        }
    }

    /// First template of each name wins; each later one is a collision warning. Sorted by name.
    fn finish(self) -> (Vec<Template>, Vec<Diagnostic>) {
        let mut diags = self.diags;
        let mut kept: Vec<Template> = Vec::new();
        for template in self.found {
            if let Some(winner) = kept.iter().find(|k| k.name == template.name) {
                diags.push(Diagnostic::warning(
                    Some(template.path.as_path()),
                    format!(
                        "prompt template \"{}\" collision, kept {}",
                        template.name,
                        winner.path.display()
                    ),
                ));
            } else {
                kept.push(template);
            }
        }
        kept.sort_by(|a, b| a.name.cmp(&b.name));
        (kept, diags)
    }
}

/// pi: the first non-empty body line (as written), cut to 60 UTF-16 units and marked with `...`
/// when longer. Empty when the body has no such line.
fn fallback_description(body: &str) -> String {
    let Some(line) = body.split('\n').find(|l| !l.trim().is_empty()) else {
        return String::new();
    };
    if utf16_len(line) <= FALLBACK_DESCRIPTION_MAX {
        return line.to_string();
    }
    let mut out = String::new();
    let mut units = 0usize;
    for c in line.chars() {
        let w = c.len_utf16();
        if units + w > FALLBACK_DESCRIPTION_MAX {
            break;
        }
        out.push(c);
        units += w;
    }
    out.push_str("...");
    out
}

/// A placeholder target inside `${...}`: all arguments, or positional argument N (1-based).
enum Target {
    All,
    Index(usize),
}

/// Try the placeholder alternatives at byte `i` (a `$`). Returns the end byte offset and the
/// replacement text, or `None` when nothing matches here. Alternatives are tried in pi's order:
/// default form, slice form, simple form.
fn match_at(s: &str, i: usize, args: &[String], all: &str) -> Option<(usize, String)> {
    let rest = &s[i..];
    if let Some(body) = rest.strip_prefix("${") {
        // `${T:-default}` where T is N, ARGUMENTS or @. The default cannot contain `}`.
        if let Some((target, tail)) = take_target(body) {
            if let Some(tail) = tail.strip_prefix(":-") {
                if let Some(close) = tail.find('}') {
                    let default = &tail[..close];
                    let value = target_value(&target, args, all);
                    let replacement = if value.is_empty() {
                        default.to_string()
                    } else {
                        value
                    };
                    let after = &tail[close + 1..];
                    return Some((s.len() - after.len(), replacement));
                }
            }
        }
        // `${@:N}` or `${@:N:L}`.
        if let Some(tail) = body.strip_prefix("@:") {
            if let Some((start_1, tail)) = take_digits(tail) {
                let (length, tail) = match tail.strip_prefix(':').and_then(take_digits) {
                    Some((len, after)) => (Some(len), after),
                    None => (None, tail),
                };
                if let Some(after) = tail.strip_prefix('}') {
                    let start = start_1.saturating_sub(1);
                    let tail_args = args.get(start..).unwrap_or(&[]);
                    let selected = match length {
                        Some(len) => tail_args.iter().take(len),
                        None => tail_args.iter().take(usize::MAX),
                    };
                    let replacement = selected.map(String::as_str).collect::<Vec<&str>>().join(" ");
                    return Some((s.len() - after.len(), replacement));
                }
            }
        }
        return None;
    }
    // Simple form: `$ARGUMENTS`, `$@`, `$N`.
    let after_dollar = &rest[1..];
    if let Some(after) = after_dollar.strip_prefix("ARGUMENTS") {
        return Some((s.len() - after.len(), all.to_string()));
    }
    if let Some(after) = after_dollar.strip_prefix('@') {
        return Some((s.len() - after.len(), all.to_string()));
    }
    let (n, after) = take_digits(after_dollar)?;
    let replacement = if n == 0 {
        String::new()
    } else {
        args.get(n - 1).cloned().unwrap_or_default()
    };
    Some((s.len() - after.len(), replacement))
}

/// The target at the start of `s`: `ARGUMENTS`, `@` or a run of digits.
fn take_target(s: &str) -> Option<(Target, &str)> {
    if let Some(after) = s.strip_prefix("ARGUMENTS") {
        return Some((Target::All, after));
    }
    if let Some(after) = s.strip_prefix('@') {
        return Some((Target::All, after));
    }
    let (n, after) = take_digits(s)?;
    Some((Target::Index(n), after))
}

fn target_value(target: &Target, args: &[String], all: &str) -> String {
    match target {
        Target::All => all.to_string(),
        Target::Index(0) => String::new(),
        Target::Index(n) => args.get(n - 1).cloned().unwrap_or_default(),
    }
}

/// A non-empty run of ASCII digits at the start of `s`, as a number (saturating).
fn take_digits(s: &str) -> Option<(usize, &str)> {
    let end = s.bytes().take_while(u8::is_ascii_digit).count();
    if end == 0 {
        return None;
    }
    let n = s[..end].parse::<usize>().unwrap_or(usize::MAX);
    Some((n, &s[end..]))
}
