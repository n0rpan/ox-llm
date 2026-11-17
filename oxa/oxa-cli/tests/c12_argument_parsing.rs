//! C12 acceptance tests: Argument parsing.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_cli::{parse_args, run_pkg, usage_text, Args, ModeArg, Subcommand};
use oxa_config::Dirs;
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

/// Inbox delivery and the commands added on main must all remain subcommands.
#[test]
fn send_status_and_work_preserve_subcommand_arguments() {
    let cases = [
        (
            ["send", "session-123", "follow up"],
            Subcommand::Send(argv(&["session-123", "follow up"])),
        ),
        (
            ["status", "session-123", "--json"],
            Subcommand::Status(argv(&["session-123", "--json"])),
        ),
        (
            ["work", "task.toml", "--offline"],
            Subcommand::Work(argv(&["task.toml", "--offline"])),
        ),
    ];
    for (words, expected) in cases {
        let args = parse(&words);
        assert_eq!(args.subcommand, Some(expected), "{words:?}");
        assert!(args.messages.is_empty(), "not a model prompt: {words:?}");
    }
}

/// A leading offline switch must not turn inbox delivery into a model prompt.
#[test]
fn offline_send_preserves_subcommand_arguments() {
    for words in [
        vec!["--offline", "send", "session-733", "follow up"],
        vec![
            "--offline",
            "--offline",
            "send",
            "session-733",
            "@literal --offline",
        ],
        vec!["--offline", "send", "session-733", "--offline"],
        vec!["send", "session-733", "--offline"],
        vec!["send", "session-733", "@literal"],
    ] {
        let args = parse(&words);
        assert_eq!(args.offline, words[0] == "--offline", "{words:?}");
        assert_eq!(
            args.subcommand,
            Some(Subcommand::Send(argv(&words[words.len() - 2..])))
        );
        assert!(args.messages.is_empty());
        assert!(args.files.is_empty());
    }
    // Explicit prompts and the option terminator must still allow the word "send".
    for words in [
        vec!["--offline", "--", "send", "session-733", "follow up"],
        vec!["--offline", "explain", "send"],
        vec!["-p", "send"],
    ] {
        let args = parse(&words);
        assert_eq!(args.subcommand, None, "{words:?}");
        assert!(args.messages.iter().any(|word| word == "send"));
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

#[test]
fn agent_options_keep_command_words_as_prompts() {
    for word in [
        "mcp",
        "install",
        "remove",
        "update",
        "list",
        "config",
        "identity",
        "report-issue",
        "wipe",
        "work",
        "auth",
        "login",
        "logout",
    ] {
        for flags in [&["--model", "m"][..], &["-c"][..]] {
            let mut words = flags.to_vec();
            words.extend([word, "example"]);
            let a = parse(&words);
            assert_eq!(a.subcommand, None, "{words:?}");
            assert_eq!(a.messages, [word, "example"]);
        }
        if word != "work" {
            let a = parse(&["--offline", word, "example"]);
            assert_eq!(a.subcommand, None, "{word}");
            assert_eq!(a.messages, [word, "example"]);
        }
    }
}

#[test]
fn only_the_offline_work_prefix_dispatches_after_options() {
    let a = parse(&["--offline", "work", "tasks.json"]);
    assert!(a.offline);
    assert_eq!(a.subcommand, Some(Subcommand::Work(argv(&["tasks.json"]))));
    for flags in [&["-p"][..], &["--model", "m"][..], &["--"][..]] {
        let mut words = vec!["--offline"];
        words.extend(flags);
        words.extend(["work", "tasks.json"]);
        let a = parse(&words);
        assert_eq!(a.subcommand, None, "{words:?}");
        assert_eq!(a.messages, ["work", "tasks.json"]);
    }
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

/// Review of #314: the last permission flag wins, also when it is `workspace` or `ask`.
#[test]
fn last_permission_flag_wins_over_an_earlier_yolo() {
    use oxa_proto::PermissionMode;
    for m in ["ask", "workspace"] {
        let a = parse(&["--yolo", "--permission-mode", m]);
        assert_eq!(a.level, Some(PermissionMode::Standard), "{m}");
    }
    let a = parse(&["--permission-mode", "ask", "--yolo"]);
    assert_eq!(a.level, Some(PermissionMode::Yolo));
}

#[test]
fn continue_consumes_an_explicit_id_and_leaves_the_new_instruction() {
    let args = parse(&["--continue=session-123", "next instruction"]);
    assert!(args.continue_);
    assert_eq!(args.continue_id.as_deref(), Some("session-123"));
    assert_eq!(args.messages, vec!["next instruction"]);
    // The spaced form is settled against the session store at dispatch.
    let args = parse(&["--continue", "session-123", "next instruction"]);
    assert!(args.continue_);
    assert_eq!(args.continue_id, None);
    assert_eq!(args.continue_word, Some(("session-123".to_string(), 0)));
    assert_eq!(args.messages, vec!["next instruction"]);
    for words in [
        vec!["--continue=session-123", "--no-session"],
        vec!["--continue=session-123", "--session", "other"],
        vec!["--continue=../escape"],
    ] {
        assert_eq!(parse_err(&words).code, codes::USAGE);
    }
}

#[test]
fn max_time_accepts_fractional_seconds_and_rejects_invalid_durations() {
    assert_eq!(
        parse(&["--max-time", "0.25", "-p", "go"]).max_time,
        Some(std::time::Duration::from_millis(250))
    );
    for value in ["0", "-1", "NaN", "inf", "1e100", "invalid"] {
        assert_eq!(parse_err(&["--max-time", value]).code, codes::USAGE, "{value}");
    }
}

#[test]
fn max_tokens_requires_a_positive_integer() {
    assert_eq!(parse(&["--max-tokens", "42", "-p", "go"]).max_tokens, Some(42));
    assert_eq!(
        parse(&["--max-tokens=18446744073709551615"]).max_tokens,
        Some(u64::MAX)
    );
    for value in ["0", "-1", "NaN", "inf", "1.5", "18446744073709551616", "invalid"] {
        assert_eq!(parse_err(&["--max-tokens", value]).code, codes::USAGE, "{value}");
    }
}

/// #874: `--trust` is the clearer spelling of `--approve` (project trust, not the tool
/// permission table) and `--output-format` of `--mode`. Both keep the old spellings.
#[test]
fn trust_and_output_format_aliases_match_their_originals() {
    // The alias parses to exactly the same options as the original spelling.
    assert_eq!(parse(&["--trust"]), parse(&["--approve"]));
    assert_eq!(parse(&["--trust"]), parse(&["-a"]));
    assert_eq!(parse(&["--trust", "hi"]), parse(&["--approve", "hi"]));
    for (alias, original) in [
        (&["--output-format", "text"][..], &["--mode", "text"][..]),
        (&["--output-format", "json"], &["--mode", "json"]),
        (&["--output-format", "rpc"], &["--mode", "rpc"]),
        (&["--output-format=json"], &["--mode=json"]),
    ] {
        assert_eq!(parse(alias), parse(original), "{alias:?} and {original:?} differ");
    }
    // The last spelling given wins, as for two `--mode` flags.
    assert_eq!(
        parse(&["--mode", "json", "--output-format", "rpc"]).mode,
        Some(ModeArg::Rpc)
    );
    assert_eq!(
        parse(&["--output-format", "rpc", "--mode", "json"]).mode,
        Some(ModeArg::Json)
    );
    // `--trust` is project trust, not the permission table: it never sets the level.
    let a = parse(&["--trust"]);
    assert_eq!(a.approve, Some(true));
    assert_eq!(a.level, None);
    assert_eq!(a.permission_mode, None);
    // Errors keep the same code and the same valid values.
    let alias = parse_err(&["--output-format", "yaml"]);
    let original = parse_err(&["--mode", "yaml"]);
    assert_eq!(alias.code, codes::USAGE);
    assert_eq!(original.code, codes::USAGE);
    for e in [&alias, &original] {
        assert!(
            e.message.contains("\"yaml\"") && e.message.contains("text, json, rpc"),
            "{}",
            e.message
        );
    }
    assert_eq!(parse_err(&["--output-format"]).code, codes::USAGE);
    assert_eq!(parse_err(&["--trust=yes"]).code, codes::USAGE);
    // The documented conflict follows the alias too.
    assert_eq!(
        parse_err(&["--print", "--output-format", "rpc"]).message,
        "--print cannot be combined with --mode rpc"
    );
}

/// #874: `oxa pkg <verb>` is the clearer spelling of the top-level package verbs.
#[test]
fn pkg_alias_delegates_the_package_verbs() {
    for (alias, original) in [
        (&["pkg", "install", "src"][..], &["install", "src"][..]),
        (&["pkg", "remove", "src"], &["remove", "src"]),
        (&["pkg", "uninstall", "src"], &["uninstall", "src"]),
        (&["pkg", "update"], &["update"]),
        (
            &["pkg", "update", "src", "--extensions"],
            &["update", "src", "--extensions"],
        ),
        (&["pkg", "list"], &["list"]),
        (&["pkg", "list", "-l"], &["list", "-l"]),
    ] {
        assert_eq!(parse(alias), parse(original), "{alias:?} and {original:?} differ");
    }
    // The verb is delegated, not validated here: the package parser reports an unknown one.
    assert_eq!(
        parse(&["pkg", "bogus"]).subcommand,
        Some(Subcommand::Pkg("bogus".to_string(), Vec::new()))
    );
    // A bare `pkg` names no verb.
    let e = parse_err(&["pkg"]);
    assert_eq!(e.code, codes::USAGE);
    assert!(e.message.contains("install"), "{}", e.message);
}

/// #874: the `pkg` alias runs the same package code with the same effect.
#[test]
fn pkg_alias_has_the_same_effect_as_the_package_verbs() {
    fn run(env: &TestEnv, words: &[&str]) -> (String, String) {
        let args = parse(words);
        let Some(Subcommand::Pkg(verb, rest)) = args.subcommand else {
            panic!("{words:?} is not a package command");
        };
        let dirs = Dirs::under(env.root());
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run_pkg(&verb, &rest, &dirs, &env.work(), &mut out, &mut err).unwrap(),
            0
        );
        assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
        let settings = std::fs::read(dirs.project_settings_file(&env.work()))
            .map(|b| String::from_utf8(b).unwrap())
            .unwrap_or_default();
        (String::from_utf8(out).unwrap(), settings)
    }
    fn source(env: &TestEnv) -> String {
        env.write(
            "package/skills/synthetic/SKILL.md",
            "---\nname: synthetic\ndescription: Synthetic fixture.\n---\nReturn a short answer.\n",
        );
        env.root().join("package").display().to_string()
    }
    let alias_env = TestEnv::new();
    let alias_src = source(&alias_env);
    let original_env = TestEnv::new();
    let original_src = source(&original_env);
    // The source path differs per env, so compare the effect with the path normalised.
    let (alias_out, alias_settings) = run(&alias_env, &["pkg", "install", "-l", &alias_src]);
    let (original_out, original_settings) = run(&original_env, &["install", "-l", &original_src]);
    assert!(alias_out.contains("(project)"), "{alias_out}");
    assert_eq!(
        alias_out.replace(&alias_src, "<src>"),
        original_out.replace(&original_src, "<src>")
    );
    assert_eq!(
        alias_settings.replace(&alias_src, "<src>"),
        original_settings.replace(&original_src, "<src>")
    );
    // Listing through the alias shows the same installed package.
    let (alias_list, _) = run(&alias_env, &["pkg", "list"]);
    let (original_list, _) = run(&original_env, &["list"]);
    assert!(alias_list.contains("synthetic"), "{alias_list}");
    assert_eq!(
        alias_list.replace(&alias_src, "<src>"),
        original_list.replace(&original_src, "<src>")
    );
}
