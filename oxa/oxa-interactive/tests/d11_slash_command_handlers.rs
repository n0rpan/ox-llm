//! D11 acceptance tests: Slash command handlers. Owner: task D11.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{Dirs, Keymap, Settings};
use oxa_core::builtin_commands;
use oxa_fake::TestEnv;
use oxa_interactive::{
    debug_dump, dispatch_slash, engine_lines, hotkeys_lines, AppAction, Block, Reopen, SelectorKind,
    SessionCmd, SlashCx, StatusModel, ViewModel, INTERACTIVE_COMMANDS,
};
use oxa_proto::{CommandSpec, Level, Source, ThinkingLevel};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Owned data behind a [`SlashCx`]. The fake env keeps every file under a temp root.
struct Fx {
    _env: TestEnv,
    vm: ViewModel,
    keymap: Keymap,
    settings: Settings,
    dirs: Dirs,
    cwd: PathBuf,
    commands: Vec<CommandSpec>,
}

impl Fx {
    fn new() -> Fx {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let cwd = env.work();
        let vm = ViewModel {
            status: StatusModel {
                model: "engine-model".to_string(),
                session_name: Some("demo".to_string()),
                n_ctx: 8192,
                context_tokens: 1200,
                total_in: 900,
                total_out: 300,
                last_cached: Some(800),
                last_prompt: Some(1000),
                ..StatusModel::default()
            },
            ..ViewModel::default()
        };
        Fx {
            _env: env,
            vm,
            keymap: Keymap::defaults(),
            settings: Settings::from_values(json!({}), json!({})),
            dirs,
            cwd,
            commands: builtin_commands(),
        }
    }

    fn cx<'a>(&'a self, busy: bool, last: Option<&'a str>) -> SlashCx<'a> {
        SlashCx {
            vm: &self.vm,
            keymap: &self.keymap,
            settings: &self.settings,
            dirs: &self.dirs,
            cwd: &self.cwd,
            commands: &self.commands,
            busy,
            session_file: None,
            session_id: "sess-1",
            last_assistant_text: last,
            engine_url: "",
            models: &[],
            queue: None,
        }
    }

    fn run(&self, line: &str) -> Option<AppAction> {
        dispatch_slash(line, &self.cx(false, None))
    }
}

fn expect_some(line: &str, action: Option<AppAction>) -> AppAction {
    action.unwrap_or_else(|| panic!("{line} should be handled by a built-in"))
}

fn notice(action: AppAction) -> (Level, String) {
    match action {
        AppAction::Notify { level, text } => (level, text),
        _ => panic!("expected a notice"),
    }
}

/// slash-commands.md list
#[test]
fn every_kept_command_has_handler() {
    let fx = Fx::new();
    // Every built-in of the registry is handled, and each handled name is listed.
    for c in builtin_commands() {
        let line = format!("/{}", c.name);
        assert!(
            INTERACTIVE_COMMANDS.contains(&c.name.as_str()),
            "{} missing from INTERACTIVE_COMMANDS",
            c.name
        );
        assert!(fx.run(&line).is_some(), "{line} has no handler");
    }
    // The commands the registry does not list still have a handler here.
    for name in ["theme", "debug", "exit"] {
        assert!(fx.run(&format!("/{name}")).is_some(), "/{name} has no handler");
    }
    // Plain text and unknown names are not commands.
    assert!(fx.run("hello /model").is_none());
    // A path is not a command.
    assert!(fx.run("/tmp/file.txt what is this").is_none());
    // Single-component absolute paths too (existing root entry, or a file name with a dot).
    assert!(fx.run("/etc explain this directory").is_none());
    assert!(fx.run("/README.md summarize this file").is_none());
}

/// #341: an unknown `/x` is rejected locally; `/help` and `/usage` answer locally.
#[test]
fn unknown_slash_is_local_error_and_help_usage_exist() {
    let fx = Fx::new();
    let (level, text) = notice(expect_some("/bogus", fx.run("/bogus")));
    assert_eq!(level, Level::Error);
    assert!(text.contains("/bogus") && text.contains("/help"), "{text}");
    match expect_some("/help", fx.run("/help")) {
        AppAction::ShowLines(l) => {
            assert!(l.iter().any(|x| x.contains("/model")));
            assert!(l.iter().any(|x| x.contains("/hotkeys")));
        }
        _ => panic!("/help shows lines"),
    }
    assert!(matches!(
        expect_some("/usage", fx.run("/usage")),
        AppAction::ShowLines(_)
    ));
    // `/cost` shows no cost, so it is not a built-in: it is an unknown command.
    let (level, _) = notice(expect_some("/cost", fx.run("/cost")));
    assert_eq!(level, Level::Error);
    // A bare dotless unknown name is a typo: rejected locally.
    let (level, _) = notice(expect_some("/zz-nope-qq", fx.run("/zz-nope-qq")));
    assert_eq!(level, Level::Error);
    // With text after it, it is a prompt (host independent): it reaches the model.
    assert!(fx.run("/zz-nope-qq go").is_none());
    assert!(fx.run("/src explain this module").is_none());
    // A template or skill command of the session still goes to the core.
    let mut fx = Fx::new();
    fx.commands.push(CommandSpec {
        name: "skill:demo".to_string(),
        description: "demo".to_string(),
        argument_hint: None,
        source: Source::Extension("x".to_string()),
    });
    assert!(fx.run("/skill:demo").is_none());
    // An extension command named like a name that is not built in reaches the core.
    fx.commands.push(CommandSpec {
        name: "deploy".to_string(),
        description: "d".to_string(),
        argument_hint: None,
        source: Source::Extension("x".to_string()),
    });
    assert!(fx.run("/deploy").is_none());
    // A template or extension named `help` / `usage` overrides the built-in handler.
    for n in ["help", "usage"] {
        let mut fx = Fx::new();
        fx.commands.retain(|c| c.name != n);
        fx.commands.push(CommandSpec {
            name: n.to_string(),
            description: "mine".to_string(),
            argument_hint: None,
            source: Source::Extension("x".to_string()),
        });
        assert!(fx.run(&format!("/{n} x")).is_none(), "/{n} override");
    }
}

/// /engine status and load
#[test]
fn engine_command() {
    let fx = Fx::new();
    match expect_some("/engine", fx.run("/engine")) {
        AppAction::ShowLines(lines) => {
            assert!(lines.iter().any(|l| l.starts_with("Endpoint: ")));
            assert!(lines.iter().any(|l| l == "Model: engine-model"));
            assert!(lines.iter().any(|l| l == "Context window: 8192 tokens"));
            assert!(lines
                .iter()
                .any(|l| l == "Cache hit, last request: 800 / 1000 tokens (80%)"));
            assert!(lines.iter().any(|l| l == "Queue: not reported"));
        }
        _ => panic!("/engine shows status lines"),
    }
    match expect_some("/engine status", fx.run("/engine status")) {
        AppAction::ShowLines(_) => {}
        _ => panic!("/engine status shows lines"),
    }
    match expect_some("/engine load qwen", fx.run("/engine load qwen")) {
        AppAction::Session(SessionCmd::SetModel(m)) => assert_eq!(m, "qwen"),
        _ => panic!("/engine load switches the model"),
    }
    let busy = dispatch_slash("/engine load qwen", &fx.cx(true, None));
    assert_eq!(notice(busy.unwrap()).0, Level::Warning);
    let (level, text) = notice(expect_some("/engine load", fx.run("/engine load")));
    assert_eq!(level, Level::Error);
    assert!(text.contains("/engine"));
    // The queue is shown when the caller knows it.
    let lines = engine_lines(&fx.vm.status, "http://127.0.0.1:8080", Some(3));
    assert!(lines.iter().any(|l| l == "Queue: 3"));
    assert!(lines.iter().any(|l| l == "Endpoint: http://127.0.0.1:8080"));
}

/// /debug
#[test]
fn debug_writes_file() {
    let fx = Fx::new();
    assert!(matches!(
        expect_some("/debug", fx.run("/debug")),
        AppAction::DebugDump
    ));
    let frame = vec!["\u{1b}[1mhello\u{1b}[0m".to_string()];
    let mut vm = ViewModel::default();
    vm.blocks.push(Block::Notice {
        level: Level::Info,
        text: "note".to_string(),
    });
    let dump = debug_dump(&frame, 100, 30, &vm);
    assert!(dump.contains("terminal: 100x30"));
    assert!(dump.contains("\\e[1mhello\\e[0m"), "escapes are made visible");
    assert!(dump.contains("--- last frame ---"));
    assert!(dump.contains("blocks: 1 "));
    assert!(!dump.contains('\u{1b}'));
    // The app writes the dump into the agent dir; the file lands inside the test root.
    let env = TestEnv::new();
    let path = env.tmp().join("debug.txt");
    std::fs::write(&path, &dump).expect("write under the test root");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), dump);
}

/// /hotkeys with overrides
#[test]
fn hotkeys_reflects_keymap() {
    let mut fx = Fx::new();
    let lines = hotkeys_lines(&fx.keymap);
    assert!(
        lines.iter().any(|l| l.contains("alt+m") && l.contains("Cycle")),
        "default mode key listed"
    );
    let unbound = lines
        .iter()
        .find(|l| l.contains("Select the previous prompt history entry"))
        .unwrap();
    assert!(unbound.contains("(unbound)"));

    // A user override replaces the default keys and the listing follows.
    let diags = fx
        .keymap
        .apply(&json!({ "app.mode.cycle": "ctrl+y", "tui.editor.historyPrevious": "ctrl+p" }));
    assert!(diags.is_empty(), "valid overrides: {diags:?}");
    let lines = hotkeys_lines(&fx.keymap);
    assert!(
        lines.iter().any(|l| l.contains("ctrl+y") && l.contains("Cycle")),
        "override shown"
    );
    assert!(
        !lines.iter().any(|l| l.contains("alt+m") && l.contains("Cycle")),
        "default gone"
    );
    let prev = lines
        .iter()
        .find(|l| l.contains("Select the previous prompt history entry"))
        .unwrap();
    assert!(prev.contains("ctrl+p") && !prev.contains("(unbound)"));
    // The same listing through the command.
    match expect_some("/hotkeys", fx.run("/hotkeys")) {
        AppAction::ShowLines(l) => assert!(l.iter().any(|x| x.contains("ctrl+y"))),
        _ => panic!("/hotkeys shows lines"),
    }
}

/// handlers
#[test]
fn compact_name_session_copy_export() {
    let fx = Fx::new();
    assert!(matches!(
        expect_some("/compact", fx.run("/compact")),
        AppAction::Session(SessionCmd::Compact { instructions: None })
    ));
    match expect_some("/compact keep the tests", fx.run("/compact keep the tests")) {
        AppAction::Session(SessionCmd::Compact {
            instructions: Some(i),
        }) => assert_eq!(i, "keep the tests"),
        _ => panic!("/compact passes its instructions"),
    }
    let busy = dispatch_slash("/compact", &fx.cx(true, None)).unwrap();
    assert_eq!(notice(busy).0, Level::Warning);

    match expect_some("/name", fx.run("/name")) {
        AppAction::ShowLines(l) => assert_eq!(l, vec!["Session name: demo".to_string()]),
        _ => panic!("/name alone shows the name"),
    }
    match expect_some("/name fixing bugs", fx.run("/name fixing bugs")) {
        AppAction::Session(SessionCmd::SetName(n)) => assert_eq!(n, "fixing bugs"),
        _ => panic!("/name sets the name"),
    }

    match expect_some("/session", fx.run("/session")) {
        AppAction::ShowLines(l) => {
            assert!(l.iter().any(|x| x == "Session id: sess-1"));
            assert!(l.iter().any(|x| x == "Tokens: input 900, output 300"));
            assert!(l.iter().any(|x| x == "Context: 1200 / 8192 tokens"));
        }
        _ => panic!("/session shows lines"),
    }

    let with_text = dispatch_slash("/copy", &fx.cx(false, Some("the answer"))).unwrap();
    assert!(matches!(with_text, AppAction::Copy(ref t) if t == "the answer"));
    let without = dispatch_slash("/copy", &fx.cx(false, None)).unwrap();
    assert_eq!(notice(without).0, Level::Warning);

    assert!(matches!(
        expect_some("/export", fx.run("/export")),
        AppAction::Export { path: None }
    ));
    match expect_some("/export out.jsonl", fx.run("/export out.jsonl")) {
        AppAction::Export { path: Some(p) } => assert_eq!(p, Path::new("out.jsonl")),
        _ => panic!("/export takes a path"),
    }
}

/// handlers
#[test]
fn new_resume_tree_fork_clone() {
    let fx = Fx::new();
    assert!(matches!(
        expect_some("/new", fx.run("/new")),
        AppAction::Session(SessionCmd::Reopen(Reopen::New))
    ));
    assert!(matches!(
        expect_some("/resume", fx.run("/resume")),
        AppAction::Open(SelectorKind::Sessions)
    ));
    assert!(matches!(
        expect_some("/tree", fx.run("/tree")),
        AppAction::Open(SelectorKind::Tree)
    ));
    assert!(matches!(
        expect_some("/fork", fx.run("/fork")),
        AppAction::Open(SelectorKind::ForkPoint)
    ));
    assert!(matches!(
        expect_some("/clone", fx.run("/clone")),
        AppAction::Session(SessionCmd::CloneSession)
    ));
    let busy = dispatch_slash("/new", &fx.cx(true, None)).unwrap();
    assert_eq!(notice(busy).0, Level::Warning);
    // Opening selectors is allowed while busy.
    assert!(matches!(
        dispatch_slash("/tree", &fx.cx(true, None)).unwrap(),
        AppAction::Open(SelectorKind::Tree)
    ));
}

/// notice
#[test]
fn reload_reports_epoch_cost() {
    let fx = Fx::new();
    match expect_some("/reload", fx.run("/reload")) {
        AppAction::Many(actions) => {
            assert!(actions
                .iter()
                .any(|a| matches!(a, AppAction::Session(SessionCmd::Reload))));
            assert!(actions
                .iter()
                .any(|a| matches!(a, AppAction::Notify { text, .. } if text.contains("epoch"))));
        }
        _ => panic!("/reload notifies and reloads"),
    }
    let busy = dispatch_slash("/reload", &fx.cx(true, None)).unwrap();
    assert_eq!(notice(busy).0, Level::Warning);
    assert!(matches!(expect_some("/quit", fx.run("/quit")), AppAction::Quit));
    assert!(matches!(expect_some("/exit", fx.run("/exit")), AppAction::Quit));
    assert!(matches!(
        expect_some("/thinking high", fx.run("/thinking high")),
        AppAction::Session(SessionCmd::SetThinking(ThinkingLevel::High))
    ));
    assert_eq!(
        notice(expect_some("/thinking loud", fx.run("/thinking loud"))).0,
        Level::Error
    );
}

/// unsupported_in_oxa
#[test]
fn omitted_commands_error() {
    let fx = Fx::new();
    for name in ["login", "logout", "share"] {
        let (level, text) = notice(expect_some(name, fx.run(&format!("/{name}"))));
        assert_eq!(level, Level::Error);
        assert!(text.contains("unsupported_in_oxa"), "{name}: {text}");
    }
    // A hook command with the same name runs first (not reserved, as pi).
    let mut fx = Fx::new();
    fx.commands.push(CommandSpec {
        name: "share".to_string(),
        description: "share it".to_string(),
        argument_hint: None,
        source: Source::Extension("ext.js".to_string()),
    });
    assert!(fx.run("/share").is_none());
}

#[test]
fn busy_refuses_only_session_changes() {
    let fx = Fx::new();
    let cx = fx.cx(true, None);
    for line in [
        "/settings",
        "/hotkeys",
        "/session",
        "/debug",
        "/quit",
        "/mode",
        "/copy",
    ] {
        assert!(
            dispatch_slash(line, &cx).is_some(),
            "{line} is allowed while busy"
        );
    }
    assert!(matches!(
        dispatch_slash("/model", &cx).unwrap(),
        AppAction::Open(SelectorKind::Model)
    ));
    assert_eq!(
        notice(dispatch_slash("/model qwen", &cx).unwrap()).0,
        Level::Warning
    );
}
