//! C12 acceptance tests: Argument parsing.
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
        "local",
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
        // Only option words may precede the work command; a prompt word before it keeps
        // `work` a prompt word in place.
        let a = parse(&["summarise", word, "example"]);
        assert_eq!(a.subcommand, None, "{word}");
        assert_eq!(a.messages, ["summarise", word, "example"]);
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

/// Extra: a `--no-<tool>` alias is `--exclude-tools <tool>`, and the resolved toolset is the
/// same for both spellings (the same exclusion precedence).
#[test]
fn no_tool_aliases_are_exclude_tools_and_pick_the_same_tools() {
    for (flag, tool) in oxa_cli::NO_TOOL_ALIASES {
        let alias = parse(&[flag]);
        assert_eq!(alias.exclude_tools, [tool], "{flag}");
        assert_eq!(alias, parse(&["--exclude-tools", tool]), "{flag}");
        // The effective selection of the real built-in tools is identical.
        let all = oxa_tools::builtin_tools();
        let (a, aw) = oxa_core::resolve_toolset(&all, &selection(&alias));
        let (b, bw) = oxa_core::resolve_toolset(&all, &selection(&parse(&["-xt", tool])));
        assert_eq!(aw, bw, "{flag}");
        let names = |set: &oxa_core::Toolset| set.specs().iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&a), names(&b), "{flag}");
        if tool != "subagent" {
            assert!(!names(&a).contains(&tool.to_string()), "{flag} kept {tool}");
        }
    }
    // Two aliases accumulate in the order given, like two `-xt` entries.
    assert_eq!(parse(&["--no-bash", "--no-read"]).exclude_tools, ["bash", "read"]);
    // A value is a usage error, as for `-xt`'s sibling flags.
    assert_eq!(parse_err(&["--no-bash=1"]).code, codes::USAGE);
}

/// A later exclusion must never undo an earlier explicit tool restriction: `-xt` and the
/// `--no-<tool>` aliases accumulate in one denylist, in either order, and further `-xt` lists
/// add to it rather than replace it.
#[test]
fn mixed_tool_exclusions_remain_disabled_in_either_order() {
    // Each case also pins the parsed list: both spellings append in the order given, so a
    // later exclusion never replaces the earlier one.
    for (words, expected) in [
        (vec!["--no-bash", "--exclude-tools", "read"], ["bash", "read"]),
        (vec!["--exclude-tools", "read", "--no-bash"], ["read", "bash"]),
        (vec!["-xt", "bash", "--exclude-tools", "read"], ["bash", "read"]),
    ] {
        let args = parse(&words);
        let (tools, warnings) = oxa_core::resolve_toolset(&oxa_tools::builtin_tools(), &selection(&args));
        assert!(warnings.is_empty(), "{words:?}: {warnings:?}");
        assert_eq!(args.exclude_tools, expected, "{words:?}");
        let names: Vec<_> = tools.specs().into_iter().map(|s| s.name).collect();
        assert!(
            !names.iter().any(|name| name == "bash"),
            "{words:?} re-enabled bash"
        );
        assert!(
            !names.iter().any(|name| name == "read"),
            "{words:?} re-enabled read"
        );
        assert!(
            names.iter().any(|name| name == "edit"),
            "{words:?} removed an unrelated tool"
        );
    }
}

/// Extra: `--animation <mode>` takes the three `ui.animation` values; anything else is a usage
/// error (exit 2 through `codes::USAGE`).
#[test]
fn animation_flag_takes_the_documented_modes_only() {
    for mode in ["arcade", "minimal", "off"] {
        assert_eq!(parse(&["--animation", mode]).animation.as_deref(), Some(mode));
    }
    assert_eq!(
        parse(&["--animation=minimal"]).animation.as_deref(),
        Some("minimal")
    );
    for bad in ["none", "Arcade", ""] {
        let e = parse_err(&["--animation", bad]);
        assert_eq!(e.code, codes::USAGE, "--animation {bad:?}");
        assert!(e.message.contains("Invalid animation"), "{bad:?}: {}", e.message);
    }
    assert_eq!(parse_err(&["--animation"]).code, codes::USAGE);
    assert_eq!(parse(&[]).animation, None);
}

fn selection(a: &Args) -> oxa_core::ToolSelection {
    oxa_core::ToolSelection {
        tools: a.tools.clone(),
        exclude: a.exclude_tools.clone(),
        no_builtin: a.no_builtin_tools,
        no_tools: a.no_tools,
        ..Default::default()
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
        "--no-<tool>",
        "--animation <mode>",
        "Extension CLI Flags:",
        "--plan",
        "--depth <value>",
    ] {
        assert!(text.contains(needle), "usage text lacks {needle:?}:\n{text}");
    }
    assert!(!usage_text(&[]).contains("Extension CLI Flags"));
}

/// The CLI command table and the registry both know `cache`; help prints it once.
#[test]
fn usage_text_lists_cache_once() {
    assert_eq!(usage_text(&[]).matches("oxa cache").count(), 1);
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
fn max_time_allowed_interactive() {
    // S11: the limits no longer refuse a terminal start; the interactive session takes them.
    let args = parse(&["--max-time", "5", "--max-tokens", "9"]);
    assert_eq!(args.max_time, Some(std::time::Duration::from_secs(5)));
    assert_eq!(args.max_tokens, Some(9));
    assert!(args.mode.is_none() && !args.print);
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
