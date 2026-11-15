//! C12 acceptance tests: Argument parsing. Owner: task C12.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_cli::{parse_args, usage_text, Args, ModeArg, Subcommand};
use oxa_fake::TestEnv;
use oxa_proto::{codes, FlagSpec, Mode, OxaError, ThinkingLevel, Value};

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|s| s.to_string()).collect()
}

fn parse(words: &[&str]) -> Args {
    parse_args(&argv(words), &[]).unwrap_or_else(|e| panic!("parse {words:?} failed: {e}"))
}

fn parse_err(words: &[&str]) -> OxaError {
    match parse_args(&argv(words), &[]) {
        Ok(a) => panic!("parse {words:?} should fail, got {a:?}"),
        Err(e) => e,
    }
}

fn spec(name: &str, boolean: bool) -> FlagSpec {
    FlagSpec {
        name: name.to_string(),
        description: format!("{name} flag"),
        boolean,
        default: Value::Null,
    }
}

/// table test, long and short spellings
#[test]
fn every_flag_of_cli_md() {
    type Check = fn(&Args) -> bool;
    let cases: &[(&[&str], Check)] = &[
        (&["-p", "hi"], |a| a.print && a.messages == ["hi"]),
        (&["--print"], |a| a.print),
        (&["--mode", "text"], |a| a.mode == Some(ModeArg::Text)),
        (&["--mode", "json"], |a| a.mode == Some(ModeArg::Json)),
        (&["--mode", "rpc"], |a| a.mode == Some(ModeArg::Rpc)),
        (&["--export", "in.jsonl", "out.html"], |a| {
            a.export == Some(("in.jsonl".to_string(), Some("out.html".to_string())))
        }),
        (&["--export", "in.jsonl"], |a| {
            a.export == Some(("in.jsonl".to_string(), None))
        }),
        (&["--provider", "openai", "--model", "gpt"], |a| {
            a.provider.as_deref() == Some("openai") && a.model.as_deref() == Some("gpt")
        }),
        (&["--model", "sonnet:high"], |a| {
            a.model.as_deref() == Some("sonnet:high")
        }),
        (&["--models", "a, b,,c"], |a| {
            a.models == Some(vec!["a".into(), "b".into(), "c".into()])
        }),
        (&["--list-models"], |a| a.list_models == Some(None)),
        (&["--list-models", "gpt"], |a| {
            a.list_models == Some(Some("gpt".to_string()))
        }),
        (&["--thinking", "max"], |a| a.thinking == Some(ThinkingLevel::Max)),
        (&["-c"], |a| a.continue_),
        (&["--continue"], |a| a.continue_),
        (&["-r"], |a| a.resume),
        (&["--resume"], |a| a.resume),
        (&["--session", "abc"], |a| a.session.as_deref() == Some("abc")),
        (&["--session-id", "abc.1"], |a| {
            a.session_id.as_deref() == Some("abc.1")
        }),
        (&["--fork", "xyz"], |a| a.fork.as_deref() == Some("xyz")),
        (&["--session-dir", "/d"], |a| {
            a.session_dir.as_deref() == Some(std::path::Path::new("/d"))
        }),
        (&["--no-session"], |a| a.no_session),
        (&["-n", "name"], |a| a.name.as_deref() == Some("name")),
        (&["--name", "name"], |a| a.name.as_deref() == Some("name")),
        (&["-t", "read,bash"], |a| {
            a.tools == Some(vec!["read".into(), "bash".into()])
        }),
        (&["--tools", "read"], |a| a.tools == Some(vec!["read".into()])),
        (&["-xt", "bash"], |a| a.exclude_tools == ["bash"]),
        (&["--exclude-tools", "bash"], |a| a.exclude_tools == ["bash"]),
        (&["-nbt"], |a| a.no_builtin_tools),
        (&["--no-builtin-tools"], |a| a.no_builtin_tools),
        (&["-nt"], |a| a.no_tools),
        (&["--no-tools"], |a| a.no_tools),
        (&["--no-mcp"], |a| a.no_mcp),
        (&["-e", "a.ts"], |a| a.extensions == ["a.ts"]),
        (&["--extension", "a.ts"], |a| a.extensions == ["a.ts"]),
        (&["-ne"], |a| a.no_extensions),
        (&["--no-extensions"], |a| a.no_extensions),
        (&["--skill", "s"], |a| a.skills == ["s"]),
        (&["-ns"], |a| a.no_skills),
        (&["--no-skills"], |a| a.no_skills),
        (&["--prompt-template", "p"], |a| a.prompt_templates == ["p"]),
        (&["-np"], |a| a.no_prompt_templates),
        (&["--no-prompt-templates"], |a| a.no_prompt_templates),
        (&["--theme", "t"], |a| a.themes == ["t"]),
        (&["--no-themes"], |a| a.no_themes),
        (&["--use-theme", "dark"], |a| {
            a.use_theme.as_deref() == Some("dark")
        }),
        (&["-nc"], |a| a.no_context_files),
        (&["--no-context-files"], |a| a.no_context_files),
        (&["--system-prompt", "x"], |a| {
            a.system_prompt.as_deref() == Some("x")
        }),
        (&["--append-system-prompt", "y"], |a| {
            a.append_system_prompt == ["y"]
        }),
        (&["--tui-mode", "regular"], |a| {
            a.tui_mode.as_deref() == Some("regular")
        }),
        (&["-a"], |a| a.approve == Some(true)),
        (&["--approve"], |a| a.approve == Some(true)),
        (&["-na"], |a| a.approve == Some(false)),
        (&["--no-approve"], |a| a.approve == Some(false)),
        (&["--permission-mode", "ask"], |a| {
            a.permission_mode == Some(Mode::Ask)
        }),
        (&["--verbose"], |a| a.verbose),
        (&["--offline"], |a| a.offline),
        (&["-h"], |a| a.help),
        (&["--help"], |a| a.help),
        (&["-v"], |a| a.version),
        (&["--version"], |a| a.version),
    ];
    for (words, check) in cases {
        let a = parse(words);
        assert!(check(&a), "flag {words:?} parsed to {a:?}");
    }
    // Long and short spellings of the same flag give the same result.
    for (long, short) in [
        (&["--print"][..], &["-p"][..]),
        (&["--continue"], &["-c"]),
        (&["--resume"], &["-r"]),
        (&["--name", "n"], &["-n", "n"]),
        (&["--tools", "read"], &["-t", "read"]),
        (&["--exclude-tools", "bash"], &["-xt", "bash"]),
        (&["--no-builtin-tools"], &["-nbt"]),
        (&["--no-tools"], &["-nt"]),
        (&["--extension", "a"], &["-e", "a"]),
        (&["--no-extensions"], &["-ne"]),
        (&["--no-skills"], &["-ns"]),
        (&["--no-prompt-templates"], &["-np"]),
        (&["--no-context-files"], &["-nc"]),
        (&["--approve"], &["-a"]),
        (&["--no-approve"], &["-na"]),
        (&["--help"], &["-h"]),
        (&["--version"], &["-v"]),
    ] {
        assert_eq!(parse(long), parse(short), "{long:?} and {short:?} differ");
    }
}

/// each conflict with pi's message
#[test]
fn documented_conflicts_rejected() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["--continue", "--resume"],
            "--continue cannot be combined with --resume",
        ),
        (
            &["--session", "a", "--no-session"],
            "--session cannot be combined with --no-session",
        ),
        (
            &["--fork", "x", "--continue"],
            "--fork cannot be combined with --continue",
        ),
        (
            &["--fork", "x", "--session", "a", "--resume"],
            "--fork cannot be combined with --session, --resume",
        ),
        (
            &["--fork", "x", "--no-session"],
            "--fork cannot be combined with --no-session",
        ),
        (
            &["--session-id", "a", "--resume"],
            "--session-id cannot be combined with --resume",
        ),
        (
            &["--session-id", "a", "--session", "b"],
            "--session-id cannot be combined with --session",
        ),
        (
            &["-p", "--mode", "rpc"],
            "--print cannot be combined with --mode rpc",
        ),
        (&["--provider", "openai"], "--provider requires --model"),
    ];
    for (words, message) in cases {
        let e = parse_err(words);
        assert_eq!(e.code, codes::USAGE, "{words:?}");
        assert!(
            e.message.starts_with(message),
            "{words:?}: got {:?}, want {message:?}",
            e.message
        );
    }
    // Allowed combinations.
    parse(&["--fork", "x", "--session-dir", "/d"]);
    parse(&["--session-id", "a", "--fork", "x"]);
    parse(&["--provider", "openai", "--model", "gpt"]);
}

/// @path
#[test]
fn at_file_collected() {
    let a = parse(&["@a.md", "hi", "@b.png"]);
    assert_eq!(a.files, ["a.md", "b.png"]);
    assert_eq!(a.messages, ["hi"]);

    // A real file path, as the caller would pass it, from an isolated test tree.
    let env = TestEnv::new();
    let path = env.write("work/notes.md", "# notes\n");
    let arg = format!("@{}", path.display());
    let a = parse_args(&argv(&[&arg, "summarise"]), &[]).expect("parse");
    assert_eq!(a.files, [path.display().to_string()]);
    assert_eq!(a.messages, ["summarise"]);
}

/// prompt starting with -
#[test]
fn double_dash_stops_parsing() {
    let a = parse(&["-p", "--", "- Summarize these", "@x.md", "--help"]);
    assert!(a.print);
    assert!(!a.help, "--help after -- is a message");
    assert_eq!(a.messages, ["- Summarize these", "--help"]);
    assert_eq!(a.files, ["x.md"]);
    // -p does not swallow the words after it; they stay positional.
    let a = parse(&["-p", "--mode", "json", "prompt"]);
    assert_eq!(a.messages, ["prompt"]);
}

/// usage error
#[test]
fn unknown_short_flag_error() {
    for words in [&["-x"][..], &["-zz"], &["-c", "-q", "hi"], &["-"]] {
        let e = parse_err(words);
        assert_eq!(e.code, codes::USAGE, "{words:?}");
        assert!(
            e.message.starts_with("Unknown option: "),
            "{words:?}: {}",
            e.message
        );
        assert!(e.hint.is_some(), "usage errors carry a hint");
    }
    assert_eq!(parse_err(&["-x"]).message, "Unknown option: -x");
}

/// unknown long flags resolved
#[test]
fn extension_flags_second_pass() {
    let words = argv(&["--plan", "fix bug", "--depth=3", "--dry"]);
    // First pass: extensions have not registered anything yet, so the flags are unknown.
    let first = parse_args(&words, &[]).expect("first pass");
    assert_eq!(
        first.unknown,
        [
            ("plan".to_string(), Some("fix bug".to_string())),
            ("depth".to_string(), Some("3".to_string())),
            ("dry".to_string(), None),
        ]
    );
    assert!(first.extension_flags.is_empty());

    // Second pass with the registered flags: a boolean flag takes no value, so its word stays a prompt.
    let specs = [spec("plan", true), spec("depth", false)];
    let second = parse_args(&words, &specs).expect("second pass");
    assert_eq!(second.messages, ["fix bug"]);
    assert_eq!(
        second.extension_flags,
        [
            ("plan".to_string(), Value::Bool(true)),
            ("depth".to_string(), Value::String("3".into()))
        ]
    );
    assert_eq!(second.unknown, [("dry".to_string(), None)]);

    // A string extension flag without a value is a usage error.
    let e = parse_args(&argv(&["--depth"]), &specs).expect_err("missing value");
    assert_eq!(e.code, codes::USAGE);
    assert_eq!(e.message, "--depth requires a value");
    // A boolean extension flag with a value is a usage error.
    let e = parse_args(&argv(&["--plan=yes"]), &specs).expect_err("boolean with value");
    assert_eq!(e.message, "--plan does not take a value");
}

/// auth (H, CLI-013: `--api-key` is the adapted bearer key, kept out of `Debug`)
#[test]
fn omitted_flags_unsupported() {
    let a = parse(&["--api-key", "sk-test", "-p", "x"]);
    assert_eq!(a.api_key.as_ref().map(|k| k.0.as_str()), Some("sk-test"));
    assert!(
        !format!("{a:?}").contains("sk-test"),
        "the key never appears in Debug output"
    );
    let e = parse_err(&["--api-key", " "]);
    assert_eq!(e.code, codes::USAGE);

    let e = parse_err(&["auth", "login", "openai"]);
    assert_eq!(e.code, codes::UNSUPPORTED_IN_OXA);
    assert!(e.message.contains("auth"));
}

/// usage error
#[test]
fn invalid_thinking_and_mode_values() {
    let e = parse_err(&["--thinking", "huge"]);
    assert_eq!(e.code, codes::USAGE);
    assert!(
        e.message.starts_with("Invalid thinking level \"huge\""),
        "{}",
        e.message
    );

    let e = parse_err(&["--mode", "yaml"]);
    assert_eq!(e.code, codes::USAGE);
    assert_eq!(e.message, "Invalid mode \"yaml\". Valid values: text, json, rpc");

    let e = parse_err(&["--mode"]);
    assert_eq!(e.code, codes::USAGE);
    assert_eq!(e.message, "--mode requires text, json, or rpc");

    assert_eq!(parse_err(&["--tui-mode", "tiny"]).code, codes::USAGE);
    assert_eq!(parse_err(&["--permission-mode", "root"]).code, codes::USAGE);
    // Tool lists: plain names cannot mix with +name / -name entries, and modifiers take no patterns.
    assert_eq!(parse_err(&["--tools", "read,+bash"]).code, codes::USAGE);
    assert_eq!(parse_err(&["--tools", "+bash*"]).code, codes::USAGE);
    // Session ids follow pi's rule.
    assert_eq!(parse_err(&["--session-id", "a."]).code, codes::USAGE);
    assert_eq!(parse_err(&["--session-id", "a b"]).code, codes::USAGE);

    // The valid spellings parse.
    for level in ThinkingLevel::ALL {
        assert_eq!(parse(&["--thinking", level.as_str()]).thinking, Some(level));
    }
    assert_eq!(
        parse(&["--permission-mode", "plan"]).permission_mode,
        Some(Mode::Plan)
    );
    assert_eq!(
        parse(&["--tools", "+codemode,-write"]).tools.unwrap(),
        ["+codemode", "-write"]
    );
}

/// mcp / install / config words
#[test]
fn subcommands_split() {
    let a = parse(&["mcp", "add", "x", "--", "npx", "-y"]);
    assert_eq!(
        a.subcommand,
        Some(Subcommand::Mcp(argv(&["add", "x", "--", "npx", "-y"])))
    );

    let a = parse(&["install", "git:github.com/scope/pkg", "-l"]);
    assert_eq!(
        a.subcommand,
        Some(Subcommand::Pkg(
            "install".into(),
            argv(&["git:github.com/scope/pkg", "-l"])
        ))
    );

    let a = parse(&["update", "--extensions"]);
    assert_eq!(
        a.subcommand,
        Some(Subcommand::Pkg("update".into(), argv(&["--extensions"])))
    );

    let a = parse(&["list"]);
    assert_eq!(a.subcommand, Some(Subcommand::Pkg("list".into(), vec![])));

    let a = parse(&["config", "-l"]);
    assert_eq!(a.subcommand, Some(Subcommand::Config(argv(&["-l"]))));

    // Only the first word is a subcommand; later words are prompts.
    let a = parse(&["-p", "mcp"]);
    assert_eq!(a.subcommand, None);
    assert_eq!(a.messages, ["mcp"]);
    let a = parse(&["summarise", "install"]);
    assert_eq!(a.subcommand, None);
    assert_eq!(a.messages, ["summarise", "install"]);
}

/// Extra: help lists the flags and the extension flags.
#[test]
fn usage_text_lists_flags_and_extension_flags() {
    let text = usage_text(&[spec("plan", true), spec("depth", false)]);
    for needle in [
        "--print, -p",
        "--export <input> [output]",
        "--permission-mode <mode>",
        "--no-context-files, -nc",
        "Extension CLI Flags:",
        "--plan",
        "--depth <value>",
    ] {
        assert!(text.contains(needle), "usage text lacks {needle:?}:\n{text}");
    }
    assert!(!usage_text(&[]).contains("Extension CLI Flags"));
}

/// Extra: a plain prompt keeps word order, and no words give the default Args.
#[test]
fn prompts_keep_order_and_default_is_empty() {
    let a = parse(&["first", "@f", "second"]);
    assert_eq!(a.messages, ["first", "second"]);
    assert_eq!(a.files, ["f"]);
    assert_eq!(parse(&[]), Args::default());
}
