//! C13 acceptance tests: Autocomplete and fuzzy matching.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_tui::{
    fuzzy_filter, fuzzy_match, AutocompleteItem, AutocompleteProvider, CombinedProvider, FileIndex,
    SlashCommand, Suggestions,
};

// ---- helpers ------------------------------------------------------------------------------

fn command(name: &str, description: Option<&str>, hint: Option<&str>) -> SlashCommand {
    SlashCommand {
        name: name.to_string(),
        description: description.map(str::to_string),
        argument_hint: hint.map(str::to_string),
        complete_args: None,
    }
}

fn one_line(text: &str) -> Vec<String> {
    vec![text.to_string()]
}

/// The provider's suggestions for a single-line editor with the cursor at the end.
fn suggest(p: &mut CombinedProvider, text: &str, force: bool) -> Option<Suggestions> {
    p.suggestions(&one_line(text), 0, text.len(), force)
}

fn values(s: &Option<Suggestions>) -> Vec<String> {
    s.as_ref()
        .map(|s| s.items.iter().map(|i| i.value.clone()).collect())
        .unwrap_or_default()
}

fn labels(s: &Option<Suggestions>) -> Vec<String> {
    s.as_ref()
        .map(|s| s.items.iter().map(|i| i.label.clone()).collect())
        .unwrap_or_default()
}

fn item(value: &str, label: &str) -> AutocompleteItem {
    AutocompleteItem {
        value: value.to_string(),
        label: label.to_string(),
        description: None,
    }
}

// ---- tests --------------------------------------------------------------------------------

/// prefix and fuzzy
#[test]
fn slash_command_completion() {
    let env = TestEnv::new();
    let commands = vec![
        command("model", Some("Switch model"), None),
        command("clear", None, None),
        command("compact", None, None),
        command("theme", None, None),
    ];
    let mut p = CombinedProvider::new(
        commands,
        FileIndex::from_paths(&env.work(), vec![]),
        &env.work(),
        &env.home(),
    );

    // A bare "/" lists every command, in input order (no query means no ranking).
    let all = suggest(&mut p, "/", false).expect("bare slash lists commands");
    assert_eq!(all.items.len(), 4);
    assert_eq!(all.prefix, "/");

    // Prefix match.
    let s = suggest(&mut p, "/mo", false).expect("prefix /mo");
    assert_eq!(values(&Some(s.clone())), vec!["model"]);
    assert_eq!(s.prefix, "/mo");
    assert_eq!(values(&suggest(&mut p, "/cl", false)), vec!["clear"]);

    // Fuzzy: m, d, l in order occur only in "model".
    assert_eq!(values(&suggest(&mut p, "/mdl", false)), vec!["model"]);

    // Applying a command name replaces the typed prefix and adds a trailing space.
    let s = suggest(&mut p, "/mo", false).unwrap();
    let applied = p.apply(&one_line("/mo"), 0, 3, &s.items[0], &s.prefix);
    assert_eq!(applied.lines, vec!["/model ".to_string()]);
    assert_eq!(applied.cursor_col, 7);

    // A typed command with no argument completions gives no suggestions once a space follows.
    assert!(suggest(&mut p, "/model ", false).is_none());
    // Unknown commands give nothing.
    assert!(suggest(&mut p, "/zzz", false).is_none());
}

/// ignored files absent
#[test]
fn at_file_fuzzy_gitignore() {
    let env = TestEnv::new();
    let work = env.work();
    env.write("work/.gitignore", "target/\n*.log\nsecret.env\n!keep.log\n");
    env.write("work/src/.gitignore", "generated/\n");
    env.write("work/src/main.rs", "fn main() {}");
    env.write("work/src/lib/util.rs", "");
    env.write("work/src/generated/x.rs", "");
    env.write("work/target/debug/app", "");
    env.write("work/build.log", "");
    env.write("work/keep.log", "");
    env.write("work/secret.env", "");
    env.write("work/notes/todo.md", "");
    env.write("work/README.md", "");
    env.write("work/.git/config", "");

    let index = FileIndex::build(&work, 10_000);
    assert!(!index.truncated);
    assert!(
        index.paths.contains(&"src/main.rs".to_string()),
        "{:?}",
        index.paths
    );
    assert!(
        index.paths.contains(&"src/lib/".to_string()),
        "directories are listed with a slash"
    );
    assert!(index.paths.contains(&"src/lib/util.rs".to_string()));
    assert!(index.paths.contains(&"notes/todo.md".to_string()));
    assert!(
        index.paths.contains(&"keep.log".to_string()),
        "a later negation re-includes"
    );
    for absent in [
        "target/",
        "target/debug/app",
        "build.log",
        "secret.env",
        "src/generated/x.rs",
        ".git/config",
    ] {
        assert!(
            !index.paths.iter().any(|p| p == absent || p.starts_with(absent)),
            "{absent} must not be indexed: {:?}",
            index.paths
        );
    }

    let mut p = CombinedProvider::new(vec![], index, &work, &env.home());

    // Fuzzy: "mai" matches src/main.rs; the inserted text is the @-path.
    let s = suggest(&mut p, "look @mai", false).expect("@mai finds main.rs");
    assert_eq!(s.prefix, "@mai");
    assert!(values(&Some(s.clone())).contains(&"@src/main.rs".to_string()));
    let main = s
        .items
        .iter()
        .find(|i| i.value == "@src/main.rs")
        .unwrap()
        .clone();
    assert_eq!(main.description.as_deref(), Some("src/main.rs"));
    let applied = p.apply(&one_line("look @mai"), 0, 9, &main, "@mai");
    assert_eq!(applied.lines, vec!["look @src/main.rs ".to_string()]);
    assert_eq!(applied.cursor_col, "look @src/main.rs ".len());

    // Subsequence match over the path: u, t, l occur in order in src/lib/util.rs.
    let s = suggest(&mut p, "@utl", false).expect("@utl finds util.rs");
    assert!(values(&Some(s)).contains(&"@src/lib/util.rs".to_string()));

    // Ignored paths never show up, not even as fuzzy candidates.
    for query in ["@build", "@target", "@secret", "@generated", "@tar"] {
        let v = values(&suggest(&mut p, query, false));
        assert!(
            !v.iter().any(|x| x.contains("build.log")
                || x.contains("target")
                || x.contains("secret")
                || x.contains("generated")),
            "{query}: {v:?}"
        );
    }

    // A directory match keeps the list open: no trailing space, slash kept.
    let s = suggest(&mut p, "@src/li", false).expect("@src/li lists lib");
    let lib = s
        .items
        .iter()
        .find(|i| i.label == "lib/")
        .expect("lib/ offered")
        .clone();
    assert_eq!(lib.value, "@src/lib/");
    let applied = p.apply(&one_line("@src/li"), 0, 7, &lib, "@src/li");
    assert_eq!(applied.lines, vec!["@src/lib/".to_string()]);
}

/// directories get a slash
#[test]
fn tab_path_completion() {
    let env = TestEnv::new();
    let work = env.work();
    env.write("work/docs/a.md", "");
    env.write("work/docs-notes.txt", "");
    env.write("work/zeta.txt", "");
    env.write("home/notes.txt", "");
    let mut p = CombinedProvider::new(vec![], FileIndex::from_paths(&work, vec![]), &work, &env.home());

    // Without Tab (force = false) a bare word after a space is not a path trigger.
    assert!(suggest(&mut p, "cat d", false).is_none());

    // Tab: directories first, with a trailing slash; files without.
    let s = p.suggestions(&one_line("cat d"), 0, 5, true).expect("Tab on d");
    assert_eq!(s.prefix, "d");
    assert_eq!(
        s.items.iter().map(|i| i.value.as_str()).collect::<Vec<_>>(),
        vec!["docs/", "docs-notes.txt"]
    );
    assert_eq!(s.items[0].label, "docs/");
    assert_eq!(s.items[1].label, "docs-notes.txt");

    // Applying a directory: the path is replaced and no space is added.
    let dir = s.items[0].clone();
    let applied = p.apply(&one_line("cat d"), 0, 5, &dir, &s.prefix);
    assert_eq!(applied.lines, vec!["cat docs/".to_string()]);
    assert_eq!(applied.cursor_col, "cat docs/".len());

    // Inside a directory, its entries are offered with the full relative value.
    let inner = values(&Some(
        p.suggestions(&one_line("cat docs/"), 0, 9, true)
            .expect("inside docs/"),
    ));
    assert_eq!(inner, vec!["docs/a.md"]);

    // Home expansion keeps `~/` in the inserted text.
    let home = values(&Some(p.suggestions(&one_line("~/no"), 0, 4, true).expect("~/no")));
    assert_eq!(home, vec!["~/notes.txt"]);

    // Plain file names need no trailing slash.
    let s = p.suggestions(&one_line("zet"), 0, 3, true).expect("zet");
    assert_eq!(labels(&Some(s)), vec!["zeta.txt"]);
}

/// autocompleteMaxVisible
#[test]
fn max_visible_setting() {
    let env = TestEnv::new();
    let paths: Vec<String> = (0..150).map(|i| format!("f{i:03}.txt")).collect();
    let commands: Vec<SlashCommand> = (0..150)
        .map(|i| command(&format!("cmd{i:03}"), None, None))
        .collect();
    let mut p = CombinedProvider::new(
        commands,
        FileIndex::from_paths(&env.work(), paths),
        &env.work(),
        &env.home(),
    );

    // Default cap is 100 candidates.
    assert_eq!(suggest(&mut p, "/cmd", false).unwrap().items.len(), 100);
    assert_eq!(suggest(&mut p, "@f", false).unwrap().items.len(), 100);

    // The setting is honoured for both providers.
    p.set_max_items(7);
    assert_eq!(suggest(&mut p, "/cmd", false).unwrap().items.len(), 7);
    assert_eq!(suggest(&mut p, "@f", false).unwrap().items.len(), 7);

    // A cap larger than the candidate set returns all of them.
    p.set_max_items(1000);
    assert_eq!(suggest(&mut p, "/cmd", false).unwrap().items.len(), 150);
}

/// pi's ranking cases
#[test]
fn fuzzy_scoring_order() {
    // Empty query matches everything with score 0.
    let m = fuzzy_match("", "anything");
    assert!(m.matches);
    assert_eq!(m.score, 0.0);

    // Not a subsequence: no match.
    assert!(!fuzzy_match("xyz", "abc").matches);
    // Case-insensitive.
    assert!(fuzzy_match("MODEL", "model").matches);

    // An exact match ranks above a longer text that contains it.
    assert_eq!(
        fuzzy_filter(&["model-extra".into(), "model".into()], "model"),
        vec![1, 0]
    );

    // Consecutive matches beat scattered ones.
    assert_eq!(fuzzy_filter(&["a-x-b".into(), "ab-x".into()], "ab"), vec![1, 0]);

    // A match at a word boundary beats one in the middle of a word.
    assert_eq!(
        fuzzy_filter(&["xxfoo".into(), "bar-foo".into()], "foo"),
        vec![1, 0]
    );

    // Swapped letters-digits retry: "ab2" is tried as "2ab" when it does not match as typed.
    assert!(!fuzzy_match("ab2", "ab").matches);
    assert!(fuzzy_match("ab2", "v2ab").matches);

    // Every space- or slash-separated token must match; they are scored together.
    let items: Vec<String> = vec!["src/main.rs".into(), "lib/main.rs".into(), "src/lib.rs".into()];
    assert_eq!(fuzzy_filter(&items, "src main"), vec![0]);
    assert_eq!(fuzzy_filter(&items, "src/ma"), vec![0]);

    // An empty or blank query keeps every item in input order.
    assert_eq!(fuzzy_filter(&items, "   "), vec![0, 1, 2]);

    // Ties keep input order (stable).
    assert_eq!(fuzzy_filter(&["abc".into(), "abc".into()], "a"), vec![0, 1]);
}

/// command argument hints
#[test]
fn argument_hint_shown() {
    let env = TestEnv::new();
    type ArgCompleter = Box<dyn Fn(&str) -> Vec<AutocompleteItem>>;
    let theme_args: ArgCompleter = Box::new(|arg: &str| {
        ["dark", "light"]
            .iter()
            .filter(|t| t.starts_with(arg))
            .map(|t| item(t, t))
            .collect()
    });
    let commands = vec![
        command("model", Some("Switch model"), Some("<id>")),
        command("theme", None, Some("<name>")),
        command("clear", Some("Clear the screen"), None),
    ];
    let mut commands = commands;
    commands[1].complete_args = Some(theme_args);
    let mut p = CombinedProvider::new(
        commands,
        FileIndex::from_paths(&env.work(), vec![]),
        &env.work(),
        &env.home(),
    );

    // The hint is shown before the description, joined by an em dash.
    let s = suggest(&mut p, "/mo", false).unwrap();
    assert_eq!(s.items[0].description.as_deref(), Some("<id> — Switch model"));

    // A hint alone is the description; a description alone is shown as is.
    let s = suggest(&mut p, "/th", false).unwrap();
    assert_eq!(s.items[0].description.as_deref(), Some("<name>"));
    let s = suggest(&mut p, "/cl", false).unwrap();
    assert_eq!(s.items[0].description.as_deref(), Some("Clear the screen"));

    // Argument completions for a command typed with a space.
    let s = suggest(&mut p, "/theme da", false).expect("theme arguments");
    assert_eq!(s.prefix, "da");
    assert_eq!(values(&Some(s.clone())), vec!["dark"]);
    let applied = p.apply(&one_line("/theme da"), 0, 9, &s.items[0], &s.prefix);
    assert_eq!(applied.lines, vec!["/theme dark".to_string()]);
    assert_eq!(values(&suggest(&mut p, "/theme ", false)), vec!["dark", "light"]);
}

/// Extensions can add command names (`set_extra`), merged after the built-in ones.
#[test]
fn extra_candidates_are_merged_after_builtins() {
    let env = TestEnv::new();
    let mut p = CombinedProvider::new(
        vec![command("model", None, None)],
        FileIndex::from_paths(&env.work(), vec![]),
        &env.work(),
        &env.home(),
    );
    p.set_extra(Box::new(|prefix: &str| {
        if "extra-cmd".starts_with(prefix) {
            vec!["extra-cmd".to_string()]
        } else {
            vec![]
        }
    }));
    assert_eq!(values(&suggest(&mut p, "/ex", false)), vec!["extra-cmd"]);
    assert_eq!(values(&suggest(&mut p, "/", false)), vec!["model", "extra-cmd"]);
}

/// Exact scores from pi's `fuzzy.ts` (run under node on the reference source): the port must
/// reproduce pi's numbers, not only its ordering.
#[test]
fn fuzzy_scores_match_pi_reference() {
    let cases: &[(&str, &str, bool, f64)] = &[
        ("", "x", true, 0.0),
        ("model", "model", true, -184.0),
        ("mdl", "model", true, -10.4),
        ("opus", "openrouter openrouter/anthropic/claude-opus", true, -23.8),
        ("ab", "a-x-b", true, -18.6),
        ("ab", "ab-x", true, -24.9),
        ("foo", "xxfoo", true, -14.1),
        ("foo", "bar-foo", true, -23.5),
        ("ab2", "v2ab", true, -9.4),
        ("ab2", "ab", false, 0.0),
        ("utl", "src/lib/util.rs", true, -10.2),
        ("MODEL", "model", true, -184.0),
        ("gpt4", "4gpt-mini", true, -54.4),
        ("xyz", "abc", false, 0.0),
        ("a", "banana", true, 0.1),
        ("lib", "src/lib/", true, -23.5),
        ("main", "lib/main.rs", true, -37.8),
    ];
    for &(query, text, matches, score) in cases {
        let m = fuzzy_match(query, text);
        assert_eq!(m.matches, matches, "{query:?} vs {text:?}");
        if matches {
            assert!(
                (m.score - score).abs() < 1e-6,
                "{query:?} vs {text:?}: got {}, pi says {score}",
                m.score
            );
        }
    }
}

// ---- first-run: prose never opens the file list -------------------------------------------

/// A work directory with dotfiles and a few project entries.
fn prose_env() -> (TestEnv, CombinedProvider) {
    let env = TestEnv::new();
    let work = env.work();
    env.write("work/.cargo/config.toml", "");
    env.write("work/.config/x.toml", "");
    env.write("work/.claude/notes.md", "");
    env.write("work/src/main.rs", "");
    env.write("work/fine.txt", "");
    let index = FileIndex::build(&work, 10_000);
    let p = CombinedProvider::new(vec![], index, &work, &env.home());
    (env, p)
}

/// An apostrophe, a quote or a trailing space in prose is no path trigger (it listed the cwd,
/// dotfiles first).
#[test]
fn prose_with_quotes_opens_no_list() {
    let (_env, mut p) = prose_env();
    for text in ["what's ", "it's \"fine\" ", "it's \"fi", "hello ", "what's"] {
        let s = suggest(&mut p, text, false);
        assert!(s.is_none(), "{text:?} opened {:?}", values(&s));
    }
}

/// Explicit triggers still open the list: `@src/`, a `./` / `~/` / `/` token, and Tab.
#[test]
fn explicit_triggers_still_open_the_list() {
    let (env, mut p) = prose_env();
    let s = suggest(&mut p, "look @src/", false).expect("@src/ lists src");
    assert!(values(&Some(s)).contains(&"@src/main.rs".to_string()));
    let s = suggest(&mut p, "cat ./sr", false).expect("./sr lists src");
    assert_eq!(values(&Some(s)), vec!["./src/"]);
    env.write("home/notes.txt", "");
    let s = suggest(&mut p, "read ~/no", false).expect("~/no");
    assert_eq!(values(&Some(s)), vec!["~/notes.txt"]);
    // Tab on a quoted prefix is still a path completion.
    let s = suggest(&mut p, "cat \"fi", true).expect("Tab on \"fi");
    assert_eq!(values(&Some(s)), vec!["\"fine.txt\""]);
    // Tab after a space lists the cwd, without dotfiles.
    let s = suggest(&mut p, "cat ", true).expect("Tab on an empty token");
    let v = labels(&Some(s));
    assert!(v.contains(&"src/".to_string()), "{v:?}");
    assert!(!v.iter().any(|l| l.starts_with('.')), "no dotfiles: {v:?}");
}

/// Dotfiles are offered only when the typed name starts with `.`.
#[test]
fn dotfiles_only_for_a_dot_prefix() {
    let (_env, mut p) = prose_env();
    let v = values(&suggest(&mut p, "@.c", false));
    for d in ["@.cargo/", "@.config/", "@.claude/"] {
        assert!(v.contains(&d.to_string()), "{d} missing: {v:?}");
    }
    let v = values(&suggest(&mut p, "@c", false));
    assert!(
        !v.iter().any(|x| x.contains("/.") || x.starts_with("@.")),
        "@c: {v:?}"
    );
    let v = values(&suggest(&mut p, "@", false));
    assert!(
        !v.iter().any(|x| x.contains("/.") || x.starts_with("@.")),
        "@: {v:?}"
    );
    // Tab paths: the same rule.
    let v = labels(&suggest(&mut p, "cat .c", true));
    assert!(v.contains(&".cargo/".to_string()), "{v:?}");
    let v = labels(&suggest(&mut p, "cat ./", false));
    assert!(!v.iter().any(|l| l.starts_with('.')), "./ hides dotfiles: {v:?}");
}
