//! Signals from tool events (plan C.2, slice X1): what a finished tool call was about, as a closed
//! set of values the scene director can match on.
//!
//! Rules for this module:
//! * Pure: no I/O, no clock, no process environment.
//! * Every value it returns is a `&'static str` taken from a table in this file. A path, a command
//!   or a tool result is read only to pick a table row; none of its text is copied into a signal.

/// One finished tool call, as the director sees it. Borrowed, never stored.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolEvent<'a> {
    /// The tool name as the model called it: `edit`, `write`, `read`, `bash`, `wait`, ...
    pub tool: &'a str,
    /// The path argument of `edit`, `write` or `read`.
    pub path: Option<&'a str>,
    /// The command argument of `bash`.
    pub command: Option<&'a str>,
    /// The exit code of a finished `bash` call.
    pub exit_code: Option<i32>,
    /// The tool result text. Only its first 2 KiB are read.
    pub output: Option<&'a str>,
    /// A PR number from the wait arguments, or from the branch name (see [`pr_from_branch`]).
    pub pr_number: Option<u64>,
}

/// The signals of one tool call. Each field is a value from a closed table, or absent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Signals {
    pub lang: Option<&'static str>,
    pub cmd: Option<&'static str>,
    pub git: Option<&'static str>,
    pub exit: Option<&'static str>,
    pub err: Option<&'static str>,
    pub pr_is: Option<&'static str>,
}

impl Signals {
    /// The signals that are set, as `(key, value)` pairs in a fixed key order.
    pub fn pairs(&self) -> Vec<(&'static str, &'static str)> {
        let fields = [
            ("lang", self.lang),
            ("cmd", self.cmd),
            ("git", self.git),
            ("exit", self.exit),
            ("err", self.err),
            ("pr_is", self.pr_is),
        ];
        fields
            .into_iter()
            .filter_map(|(key, value)| value.map(|v| (key, v)))
            .collect()
    }
}

/// The signals of one tool call.
pub fn signals_of(event: &ToolEvent<'_>) -> Signals {
    let mut signals = Signals::default();
    if matches!(event.tool, "edit" | "write" | "read") {
        signals.lang = event.path.and_then(lang_of_path);
    }
    if event.tool == "bash" {
        if let Some(command) = event.command {
            let segments = split_segments(command);
            signals.cmd = segments.iter().find_map(|words| cmd_of_words(words));
            signals.git = segments.iter().find_map(|words| git_of_words(words));
        }
    }
    signals.exit = event.exit_code.and_then(exit_class);
    signals.err = event.output.and_then(err_class);
    signals.pr_is = event.pr_number.and_then(pr_class);
    signals
}

/// Extension or file name to a language, from a fixed table. Only the last path component is read.
const LANG_BY_EXT: &[(&str, &str)] = &[
    ("rs", "rs"),
    ("py", "py"),
    ("pyi", "py"),
    ("ts", "ts"),
    ("tsx", "ts"),
    ("js", "js"),
    ("jsx", "js"),
    ("mjs", "js"),
    ("cjs", "js"),
    ("go", "go"),
    ("c", "c"),
    ("h", "c"),
    ("cc", "cpp"),
    ("cpp", "cpp"),
    ("cxx", "cpp"),
    ("hpp", "cpp"),
    ("hh", "cpp"),
    ("sh", "sh"),
    ("bash", "sh"),
    ("zsh", "sh"),
    ("md", "md"),
    ("json", "json"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("toml", "toml"),
    ("sql", "sql"),
    ("html", "html"),
    ("htm", "html"),
    ("css", "css"),
    ("scss", "css"),
    ("nix", "nix"),
    ("lua", "lua"),
    ("zig", "zig"),
];

/// The language of a path: by extension from [`LANG_BY_EXT`], or by a well-known file name.
pub fn lang_of_path(path: &str) -> Option<&'static str> {
    let name = path.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    if name == "dockerfile" || name == "containerfile" || name.starts_with("dockerfile.") {
        return Some("docker");
    }
    if name == "pkgbuild" {
        return Some("pkgbuild");
    }
    let lower = path.to_ascii_lowercase();
    if name.ends_with(".conf") && (name.starts_with("hypr") || lower.contains("hypr/")) {
        return Some("hypr");
    }
    let (_, ext) = name.rsplit_once('.')?;
    LANG_BY_EXT
        .iter()
        .find(|(known, _)| *known == ext)
        .map(|(_, lang)| *lang)
}

/// Command families, matched on the first words of a simple command. Each pattern is words
/// separated by single spaces; a pattern matches when the command starts with those words.
const CMD_TABLE: &[(&str, &str)] = &[
    ("cargo test", "cargo-test"),
    ("cargo nextest", "cargo-test"),
    ("cargo build", "cargo-build"),
    ("cargo clippy", "clippy"),
    ("cargo fmt", "fmt"),
    ("rustfmt", "fmt"),
    ("pytest", "pytest"),
    ("python -m pytest", "pytest"),
    ("python3 -m pytest", "pytest"),
    ("go test", "go-test"),
    ("jest", "jest"),
    ("npx jest", "jest"),
    ("vitest", "vitest"),
    ("npx vitest", "vitest"),
    ("make", "make"),
    ("npm install", "npm-install"),
    ("npm i", "npm-install"),
    ("npm ci", "npm-install"),
    ("pnpm install", "npm-install"),
    ("yarn install", "npm-install"),
    ("docker build", "docker-build"),
    ("pacman", "pacman"),
    ("pip install", "pip"),
    ("pip3 install", "pip"),
];

/// Git subcommands with a signal of their own. `push` is handled apart (force only).
const GIT_TABLE: &[(&str, &str)] = &[
    ("rebase", "rebase"),
    ("cherry-pick", "cherry-pick"),
    ("bisect", "bisect"),
    ("stash", "stash"),
    ("merge", "merge"),
    ("commit", "commit"),
    ("blame", "blame"),
    ("reflog", "reflog"),
];

/// The exit codes with a signal. Any other code has none.
pub fn exit_class(code: i32) -> Option<&'static str> {
    match code {
        127 => Some("127"),
        130 => Some("130"),
        137 => Some("137"),
        139 => Some("139"),
        _ => None,
    }
}

/// Error kinds, matched as a lower-case substring of the first 2 KiB of a tool result. Rows are
/// tried in order, so the first row that matches gives the kind.
const ERR_TABLE: &[(&str, &str)] = &[
    ("traceback", "traceback (most recent call last)"),
    ("borrow", "cannot borrow"),
    ("borrow", "does not live long enough"),
    ("borrow", "use of moved value"),
    ("enospc", "no space left on device"),
    ("eaddrinuse", "address already in use"),
    ("eacces", "permission denied"),
    ("linker", "undefined reference to"),
    ("linker", "collect2: error"),
    ("dns", "could not resolve host"),
    ("dns", "name or service not known"),
    ("dns", "temporary failure in name resolution"),
    ("oom", "out of memory"),
    ("oom", "cannot allocate memory"),
    ("http-401", "401 unauthorized"),
    ("http-403", "403 forbidden"),
    ("http-404", "404 not found"),
    ("http-429", "429 too many requests"),
    ("http-5xx", "500 internal server error"),
    ("http-5xx", "502 bad gateway"),
    ("http-5xx", "503 service unavailable"),
    ("http-5xx", "504 gateway timeout"),
    ("timeout", "timed out"),
    ("timeout", "deadline exceeded"),
];

/// The error kind of a tool result, from [`ERR_TABLE`]. Only the first 2 KiB are read.
pub fn err_class(output: &str) -> Option<&'static str> {
    let head = head_bytes(output, 2048).to_ascii_lowercase();
    ERR_TABLE
        .iter()
        .find(|(_, needle)| head.contains(*needle))
        .map(|(kind, _)| *kind)
}

/// The PR number class: the named numbers first, then the shape of the number. `None` when the
/// number has no class.
pub fn pr_class(number: u64) -> Option<&'static str> {
    if number == 404 {
        return Some("404");
    }
    if number == 1337 {
        return Some("1337");
    }
    if number >= 100 && number.is_multiple_of(100) {
        return Some("round");
    }
    let digits = number.to_string();
    if digits.len() >= 2 && digits.chars().rev().eq(digits.chars()) {
        return Some("palindrome");
    }
    if number > 1 && number.is_power_of_two() {
        return Some("power-of-two");
    }
    None
}

/// The PR number of an `agent/<n>-<slug>` branch name, or `None` for any other shape.
pub fn pr_from_branch(branch: &str) -> Option<u64> {
    let rest = branch.strip_prefix("agent/")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = &rest[digits.len()..];
    if !(after.is_empty() || after.starts_with('-')) {
        return None;
    }
    digits.parse().ok()
}

/// The first `limit` bytes of `text`, cut back to a character boundary.
fn head_bytes(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Splits a shell command into simple commands and each into words. Quotes are removed from the
/// words. Unquoted `;`, `&`, `|`, `(`, `)`, backtick and newline end a simple command; `>&` and
/// `<&` are redirections and do not.
fn split_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut previous: Option<char> = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        let before = previous;
        previous = Some(c);
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            } else {
                word.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                in_word = true;
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
                in_word = true;
            }
            ' ' | '\t' => end_word(&mut word, &mut in_word, &mut words),
            '&' if matches!(before, Some('>') | Some('<')) => {
                word.push(c);
                in_word = true;
            }
            ';' | '|' | '&' | '\n' | '(' | ')' | '`' => {
                end_word(&mut word, &mut in_word, &mut words);
                end_segment(&mut words, &mut segments);
            }
            _ => {
                word.push(c);
                in_word = true;
            }
        }
    }
    end_word(&mut word, &mut in_word, &mut words);
    end_segment(&mut words, &mut segments);
    segments
}

fn end_word(word: &mut String, in_word: &mut bool, words: &mut Vec<String>) {
    if *in_word {
        words.push(std::mem::take(word));
    }
    *in_word = false;
}

fn end_segment(words: &mut Vec<String>, segments: &mut Vec<Vec<String>>) {
    let done = std::mem::take(words);
    if !done.is_empty() {
        segments.push(done);
    }
}

/// True for a `NAME=value` word (an environment assignment before a command).
fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// The words of a simple command without leading assignments and plain wrappers.
fn strip_prefix_words(words: &[String]) -> &[String] {
    let mut i = 0;
    while i < words.len()
        && (is_assignment(&words[i])
            || matches!(words[i].as_str(), "env" | "time" | "nice" | "command" | "exec"))
    {
        i += 1;
    }
    &words[i..]
}

/// True when `words` starts with the words of `pattern` (a space-separated list).
fn words_start_with(words: &[String], pattern: &str) -> bool {
    let want: Vec<&str> = pattern.split(' ').collect();
    want.len() <= words.len() && want.iter().zip(words.iter()).all(|(p, w)| *p == w.as_str())
}

fn cmd_of_words(words: &[String]) -> Option<&'static str> {
    let words = strip_prefix_words(words);
    CMD_TABLE
        .iter()
        .find(|(pattern, _)| words_start_with(words, pattern))
        .map(|(_, value)| *value)
}

/// The git signal of a simple command: the subcommand after `git` and its global options.
fn git_of_words(words: &[String]) -> Option<&'static str> {
    let words = strip_prefix_words(words);
    if words.first().map(String::as_str) != Some("git") {
        return None;
    }
    let mut i = 1;
    while i < words.len() {
        let word = words[i].as_str();
        if matches!(word, "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace") {
            i += 2;
        } else if word.starts_with('-') {
            i += 1;
        } else {
            break;
        }
    }
    let sub = words.get(i)?.as_str();
    if sub == "push" {
        let forced = words[i + 1..]
            .iter()
            .any(|w| w.as_str() == "-f" || w.starts_with("--force"));
        return if forced { Some("force-push") } else { None };
    }
    GIT_TABLE
        .iter()
        .find(|(name, _)| *name == sub)
        .map(|(_, value)| *value)
}
