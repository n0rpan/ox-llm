//! A10: path containment and read-only command classification.
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

/// Symlink expansions allowed while resolving one path (Linux's own limit is 40).
const MAX_SYMLINK_HOPS: usize = 40;

/// Absolute, lexically normalised form of `path` (relative paths resolve against `cwd`; `.`
/// and `..` removed without touching the file system).
pub fn normalize(path: &Path, cwd: &Path) -> PathBuf {
    let abs = cwd.join(path);
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::Prefix(p) => {
                out = PathBuf::new();
                out.push(p.as_os_str());
            }
            Component::RootDir => {
                out.push(c.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(std::path::MAIN_SEPARATOR_STR);
    }
    out
}

/// `true` when `path` is `root` or below it after resolving symlinks on the longest existing
/// ancestor of `path` (a not-yet-existing file is judged by its parent). `..` escapes and
/// symlinks pointing outside are outside.
pub fn is_inside(path: &Path, root: &Path) -> bool {
    let root_abs = match absolute(root) {
        Some(p) => p,
        None => return false,
    };
    // A relative path is taken from `root`, the session's working directory.
    let path_abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root_abs.join(path)
    };
    let (Some(real_root), Some(real_path)) = (resolve_real(&root_abs), resolve_real(&path_abs)) else {
        return false;
    };
    real_path.starts_with(&real_root)
}

/// Conservative allow-list for plan mode: `true` only when every simple command of the line
/// (split on `;`, `&&`, `||`, `|`, newlines) is a known read-only command (`ls`, `cat`,
/// `head`, `tail`, `wc`, `grep`, `rg`, `find` without `-delete`/`-exec`, `git` with
/// `status`/`log`/`diff`/`show`/`branch`/`rev-parse`, `pwd`, `echo`, `which`, `file`, `stat`,
/// `du`, `df`, `tree`, `sort`, `uniq`, `cut`, `diff`, `env`, `date`, `true`) and the line has
/// no redirection to a file, no command substitution, no backticks and no `&`. Anything it
/// cannot parse is not read-only. (Plan mode also mounts cwd read-only: this list only
/// decides Allow vs Deny, the sandbox enforces.)
///
/// Stricter than the list alone, on purpose: options that write files or run programs
/// (`find -exec`, `sort -o`, `tree -o`, `file -C`, `date -s`, `rg --pre`, `git diff --output`,
/// `git branch <name>`) are refused; `env` is allowed only without arguments; `uniq` only with
/// one file operand; any `<` or `>` redirection, `(`/`)`, and unquoted `$(` make the line
/// not read-only.
pub fn is_read_only_command(command: &str) -> bool {
    let Some(simple) = split_simple_commands(command) else {
        return false;
    };
    simple.iter().all(|words| read_only_words(words))
}

// ---------------------------------------------------------------- containment helpers

/// `path` as an absolute path (relative paths are taken from the current directory).
fn absolute(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

/// The path with every symlink and `..` resolved the way the kernel resolves it, for the
/// components that exist; the non-existing tail is kept as written (lexically normalised).
/// `None` on a symlink loop or an unreadable link.
pub(crate) fn resolve_real(path: &Path) -> Option<PathBuf> {
    let mut stack: VecDeque<OsString> = VecDeque::new();
    let mut resolved = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => {
                resolved = PathBuf::new();
                resolved.push(p.as_os_str());
            }
            Component::RootDir => resolved.push(std::path::MAIN_SEPARATOR_STR),
            Component::CurDir => {}
            Component::ParentDir => stack.push_back(OsString::from("..")),
            Component::Normal(s) => stack.push_back(s.to_os_string()),
        }
    }
    if resolved.as_os_str().is_empty() {
        return None; // not absolute
    }
    let mut hops = 0usize;
    while let Some(seg) = stack.pop_front() {
        if seg == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&seg);
        match std::fs::symlink_metadata(&candidate) {
            Ok(md) if md.file_type().is_symlink() => {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return None;
                }
                let target = std::fs::read_link(&candidate).ok()?;
                let mut expanded: Vec<OsString> = Vec::new();
                if target.is_absolute() {
                    resolved = PathBuf::from(std::path::MAIN_SEPARATOR_STR);
                }
                for c in target.components() {
                    match c {
                        Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
                        Component::ParentDir => expanded.push(OsString::from("..")),
                        Component::Normal(s) => expanded.push(s.to_os_string()),
                    }
                }
                for s in expanded.into_iter().rev() {
                    stack.push_front(s);
                }
            }
            _ => resolved = candidate,
        }
    }
    Some(resolved)
}

// ---------------------------------------------------------------- command classification

/// Splits a command line into simple commands, each a list of unquoted words. `None` when the
/// line uses anything plan mode refuses: `&` (not `&&`), `<`, `>`, `(`, `)`, backticks, `$(`,
/// an empty command in the middle of the line, or a quote or escape left open.
fn split_simple_commands(line: &str) -> Option<Vec<Vec<String>>> {
    #[derive(PartialEq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let chars: Vec<char> = line.chars().collect();
    let mut commands: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    let mut quote = Quote::None;
    let mut last_sep: Option<char> = None;
    let mut i = 0usize;

    // Ends the current command at a separator; an empty command is only legal as a trailing
    // `;` or newline, which is checked at the end of the line.
    fn end_command(
        words: &mut Vec<String>,
        commands: &mut Vec<Vec<String>>,
        word: &mut Option<String>,
    ) -> Option<()> {
        if let Some(w) = word.take() {
            words.push(w);
        }
        if words.is_empty() {
            return None;
        }
        commands.push(std::mem::take(words));
        Some(())
    }

    while i < chars.len() {
        let c = chars[i];
        match quote {
            Quote::Single => {
                if c == '\'' {
                    quote = Quote::None;
                } else {
                    word.get_or_insert_with(String::new).push(c);
                }
                i += 1;
            }
            Quote::Double => {
                match c {
                    '"' => quote = Quote::None,
                    '\\' => {
                        let next = *chars.get(i + 1)?;
                        word.get_or_insert_with(String::new).push(next);
                        i += 1;
                    }
                    '`' => return None,
                    '$' if chars.get(i + 1) == Some(&'(') => return None,
                    _ => word.get_or_insert_with(String::new).push(c),
                }
                i += 1;
            }
            Quote::None => {
                match c {
                    '\\' => {
                        let next = *chars.get(i + 1)?;
                        word.get_or_insert_with(String::new).push(next);
                        i += 2;
                        continue;
                    }
                    '\'' => {
                        quote = Quote::Single;
                        word.get_or_insert_with(String::new);
                    }
                    '"' => {
                        quote = Quote::Double;
                        word.get_or_insert_with(String::new);
                    }
                    '`' | '(' | ')' | '<' | '>' => return None,
                    '$' if chars.get(i + 1) == Some(&'(') => return None,
                    '&' => {
                        if chars.get(i + 1) == Some(&'&') {
                            end_command(&mut words, &mut commands, &mut word)?;
                            last_sep = Some('&');
                            i += 2;
                            continue;
                        }
                        return None;
                    }
                    '|' => {
                        end_command(&mut words, &mut commands, &mut word)?;
                        last_sep = Some('|');
                        i += if chars.get(i + 1) == Some(&'|') { 2 } else { 1 };
                        continue;
                    }
                    ';' | '\n' => {
                        end_command(&mut words, &mut commands, &mut word)?;
                        last_sep = Some(c);
                    }
                    ' ' | '\t' => {
                        if let Some(w) = word.take() {
                            words.push(w);
                        }
                    }
                    _ => word.get_or_insert_with(String::new).push(c),
                }
                i += 1;
            }
        }
    }
    if quote != Quote::None {
        return None;
    }
    if let Some(w) = word.take() {
        words.push(w);
    }
    if words.is_empty() {
        // Nothing after the last separator: legal only after a single `;` or newline, and
        // only when at least one command exists.
        return match (last_sep, commands.is_empty()) {
            (Some(';') | Some('\n'), false) => Some(commands),
            (None, false) => Some(commands),
            _ => None,
        };
    }
    commands.push(words);
    Some(commands)
}

/// `true` when `value` is a short-option cluster (`-abc`, not `--long`) containing `letter`.
fn short_has(value: &str, letter: char) -> bool {
    value.starts_with('-') && !value.starts_with("--") && value[1..].contains(letter)
}

fn any_short(args: &[String], letter: char) -> bool {
    args.iter().any(|a| short_has(a, letter))
}

fn any_long(args: &[String], prefix: &str) -> bool {
    args.iter().any(|a| a.starts_with(prefix))
}

fn read_only_words(words: &[String]) -> bool {
    let Some((name, args)) = words.split_first() else {
        return false;
    };
    let args: &[String] = args;
    match name.as_str() {
        "ls" | "cat" | "head" | "tail" | "wc" | "grep" | "cut" | "diff" | "pwd" | "echo" | "which"
        | "stat" | "du" | "df" | "true" => true,
        "tree" => !any_short(args, 'o') && !any_short(args, 'R'),
        "file" => !any_short(args, 'C') && !any_long(args, "--compile"),
        "date" => !any_short(args, 's') && !any_long(args, "--set"),
        "sort" => {
            !any_short(args, 'o') && !any_long(args, "--output") && !any_long(args, "--compress-program")
        }
        "uniq" => args.iter().filter(|a| !a.starts_with('-')).count() <= 1,
        "rg" => !any_long(args, "--pre"),
        "find" => !args.iter().any(|a| {
            matches!(
                a.as_str(),
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
                    | "-fls"
            )
        }),
        "env" => args.is_empty(),
        "git" => read_only_git(args),
        _ => false,
    }
}

fn read_only_git(args: &[String]) -> bool {
    let Some((sub, rest)) = args.split_first() else {
        return false;
    };
    match sub.as_str() {
        "status" | "rev-parse" => true,
        "log" | "show" | "diff" => !any_long(rest, "--output"),
        "branch" => rest.iter().all(|a| {
            matches!(
                a.as_str(),
                "-a" | "--all"
                    | "-r"
                    | "--remotes"
                    | "-v"
                    | "-vv"
                    | "--verbose"
                    | "-l"
                    | "--list"
                    | "--show-current"
                    | "--no-color"
            ) || a.starts_with("--color")
        }),
        _ => false,
    }
}
