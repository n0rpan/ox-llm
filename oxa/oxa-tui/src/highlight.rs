//! B11: syntax highlighting of code.
// ported from pi packages/coding-agent/src/modes/interactive/theme/theme.ts (highlightCode, getLanguageFromPath)
use crate::Theme;
use std::sync::OnceLock;
use syntect::easy::ScopeRegionIterator;
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Token for text without a mapped scope (and for every line of an unknown language).
const PLAIN: &str = "mdCodeBlock";

/// The bundled syntax set, loaded once on first use.
static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();

fn syntaxes() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// V022 (#884): load the bundled syntax set now, so the first code block does not pay for it.
///
/// One bounded load per process: the set lives in a `OnceLock`, so a second call is a no-op and
/// the highlight output is the same whether or not this ran. `true` when this call did the load.
/// The caller decides *when*: the interactive loop is meant to call it after the first editable
/// frame, off the UI thread, so startup never waits for it. Nothing calls it at startup.
pub fn warm_syntaxes() -> bool {
    warm_syntaxes_with(&SYNTAXES, SyntaxSet::load_defaults_newlines)
}

fn warm_syntaxes_with(slot: &OnceLock<SyntaxSet>, load: impl FnOnce() -> SyntaxSet) -> bool {
    if slot.get().is_some() {
        return false;
    }
    slot.set(load()).is_ok()
}

/// V022 (#884): whether the bundled syntax set is loaded (the warm-up ran, or a highlight did).
pub fn syntaxes_loaded() -> bool {
    SYNTAXES.get().is_some()
}

/// pi-style language names that syntect knows only by another token (extension or name).
fn alias(lang: &str) -> &str {
    match lang {
        "typescript" | "tsx" => "ts",
        "javascript" | "jsx" | "mjs" | "cjs" => "js",
        "python" => "py",
        "ruby" => "rb",
        "rust" => "rs",
        "csharp" => "cs",
        "bash" | "zsh" | "shell" => "sh",
        "powershell" => "ps1",
        "markdown" => "md",
        "kotlin" => "kt",
        "yml" => "yaml",
        "golang" => "go",
        "c++" => "cpp",
        _ => lang,
    }
}

/// The syntax for a fence info string or a language name. The first word counts, so
/// `rust title=x` finds Rust. `None` for an empty or unknown name.
fn find_syntax(lang: &str) -> Option<&'static SyntaxReference> {
    let token = lang.split_whitespace().next()?;
    let ss = syntaxes();
    ss.find_syntax_by_token(token)
        .or_else(|| ss.find_syntax_by_token(alias(&token.to_ascii_lowercase())))
}

/// The theme token for one syntax scope, or `None` when the scope is not mapped.
fn token_for_scope(scope: &str) -> Option<&'static str> {
    if scope.starts_with("comment") {
        Some("syntaxComment")
    } else if scope.starts_with("string") {
        Some("syntaxString")
    } else if scope.starts_with("keyword.operator") {
        Some("syntaxOperator")
    } else if scope.starts_with("keyword")
        || scope.starts_with("storage")
        || scope.starts_with("constant.language")
    {
        Some("syntaxKeyword")
    } else if scope.starts_with("constant") {
        Some("syntaxNumber")
    } else if scope.starts_with("entity.name.function")
        || scope.starts_with("support.function")
        || scope.starts_with("variable.function")
    {
        Some("syntaxFunction")
    } else if scope.starts_with("entity.name.type")
        || scope.starts_with("entity.name.class")
        || scope.starts_with("support.type")
        || scope.starts_with("support.class")
    {
        Some("syntaxType")
    } else if scope.starts_with("variable") {
        Some("syntaxVariable")
    } else if scope.starts_with("punctuation") {
        Some("syntaxPunctuation")
    } else {
        None
    }
}

/// The token of the innermost mapped scope on `stack`, or [`PLAIN`].
fn token_for_stack(stack: &ScopeStack) -> &'static str {
    for scope in stack.as_slice().iter().rev() {
        let name = scope.build_string();
        if let Some(token) = token_for_scope(&name) {
            return token;
        }
    }
    PLAIN
}

/// Every line of `code` in the plain code colour (no tokenising).
fn plain_lines(code: &str, theme: &Theme) -> Vec<String> {
    code.lines().map(|l| theme.fg(PLAIN, l)).collect()
}

/// Tokenise `code` with `syntax`. A line the grammar cannot parse is emitted plain.
fn highlight_with(syntax: &SyntaxReference, code: &str, theme: &Theme) -> Vec<String> {
    let ss = syntaxes();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::new();
    for raw in LinesWithEndings::from(code) {
        let ops = match state.parse_line(raw, ss) {
            Ok(ops) => ops,
            Err(_) => {
                out.push(theme.fg(PLAIN, raw.trim_end_matches(['\r', '\n'])));
                continue;
            }
        };
        let mut line = String::new();
        for (region, op) in ScopeRegionIterator::new(&ops, raw) {
            // A failing op leaves the stack as it was; the text is still shown.
            let _ = stack.apply(op);
            let text: String = region.chars().filter(|c| *c != '\n' && *c != '\r').collect();
            if text.is_empty() {
                continue;
            }
            line.push_str(&theme.fg(token_for_stack(&stack), &text));
        }
        out.push(line);
    }
    out
}

/// Highlight `code` and return one styled line per input line (no wrapping, no truncation).
///
/// Uses `syntect`'s bundled syntaxes for tokenising and maps scopes to the theme's `syntax*`
/// tokens (comment, keyword, function, variable, string, number, type, operator, punctuation);
/// text without a mapped scope uses `mdCodeBlock`. `lang` is a fence info string or a name
/// from [`language_from_path`]; an unknown or missing language returns the lines in
/// `mdCodeBlock` unhighlighted. Never panics; deterministic. Tabs are replaced by 3 spaces.
pub fn highlight_code(code: &str, lang: Option<&str>, theme: &Theme) -> Vec<String> {
    let code = code.replace('\t', "   ");
    match lang.and_then(find_syntax) {
        Some(syntax) => highlight_with(syntax, &code, theme),
        None => plain_lines(&code, theme),
    }
}

/// The language name for a file path by extension or file name (`foo.rs` -> `rust`,
/// `Makefile` -> `makefile`), pi's table. `None` when unknown.
pub fn language_from_path(path: &str) -> Option<&'static str> {
    let base = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    if base == "cmakelists.txt" {
        return Some("cmake");
    }
    // A name without a dot is its own "extension" (`Makefile`, `Dockerfile`), as in pi.
    let ext = base.rsplit('.').next()?;
    let lang = match ext {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "dart" => "dart",
        "groovy" | "gradle" => "groovy",
        "nix" => "nix",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "fish" => "fish",
        "ps1" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "dockerfile" => "dockerfile",
        "makefile" => "makefile",
        "cmake" => "cmake",
        "lua" => "lua",
        "perl" => "perl",
        "r" => "r",
        "scala" => "scala",
        "clj" => "clojure",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "ml" => "ocaml",
        "vim" => "vim",
        "graphql" => "graphql",
        "proto" => "protobuf",
        "tf" | "hcl" => "hcl",
        _ => return None,
    };
    Some(lang)
}

/// `true` when [`highlight_code`] knows `lang`.
pub fn language_supported(lang: &str) -> bool {
    find_syntax(lang).is_some()
}

#[cfg(test)]
mod warm_tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Barrier,
    };

    #[test]
    fn racing_warmups_execute_one_loader_and_report_one_initializer() {
        let slot = OnceLock::new();
        let loads = AtomicUsize::new(0);
        let start = Barrier::new(8);
        let initialized = std::thread::scope(|scope| {
            let calls: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        warm_syntaxes_with(&slot, || {
                            loads.fetch_add(1, Ordering::SeqCst);
                            SyntaxSet::load_defaults_newlines()
                        })
                    })
                })
                .collect();
            calls
                .into_iter()
                .map(|call| call.join().unwrap())
                .filter(|initialized| *initialized)
                .count()
        });
        assert_eq!(initialized, 1, "only the initializer reports true");
        assert_eq!(loads.load(Ordering::SeqCst), 1, "the expensive loader runs once");
        assert!(!warm_syntaxes_with(&slot, || panic!("a warm set is not rebuilt")));
    }
}
