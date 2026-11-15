//! D6 acceptance tests: Footer, header, title. Owner: task D6.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.
//! (The chrome functions are pure: no file or directory is touched, so no TestEnv is needed.)

use oxa_config::Keymap;
use oxa_interactive::{
    activity_text, editor_border_token, key_hints, render_footer, render_header, title_text, Activity,
    HeaderInfo, StatusModel, ViewModel,
};
use oxa_proto::{Mode, ThinkingLevel};
use oxa_tui::{Color, Theme};

/// A theme with a few colours set, so the SGR output of tokens is visible in tests.
fn coloured() -> Theme {
    let mut theme = Theme::plain();
    theme.colors.insert("error".to_string(), Color::Indexed(1));
    theme.colors.insert("modePlan".to_string(), Color::Indexed(3));
    theme
}

fn vm_with(status: StatusModel) -> ViewModel {
    ViewModel {
        status,
        ..ViewModel::default()
    }
}

fn base_status() -> StatusModel {
    StatusModel {
        cwd: "/work/proj".to_string(),
        model: "qwen".to_string(),
        ..StatusModel::default()
    }
}

/// numbers from Stats
#[test]
fn footer_context_percent_and_cache_hit() {
    let st = StatusModel {
        context_tokens: 2000,
        n_ctx: 8000,
        last_cached: Some(600),
        last_prompt: Some(800),
        total_in: 5000,
        total_out: 300,
        ..base_status()
    };
    let lines = render_footer(&vm_with(st), 120, &Theme::plain());
    assert_eq!(lines.len(), 2, "no extension status, so two lines");
    let stats = &lines[1];
    assert!(stats.contains("25.0%/8.0k"), "context percent of n_ctx: {stats}");
    assert!(
        stats.contains("cache 600/800"),
        "cache hit of the last request: {stats}"
    );
    assert!(
        stats.contains("↑5.0k") && stats.contains("↓300"),
        "token totals: {stats}"
    );
    assert!(stats.contains("qwen"), "model on the right: {stats}");
}

/// Prefill events
#[test]
fn prefill_progress() {
    let st = StatusModel {
        activity: Activity::Prefill {
            done: 1200,
            total: 8000,
            secs: 3,
        },
        ..base_status()
    };
    let lines = render_footer(&vm_with(st.clone()), 120, &Theme::plain());
    assert!(
        lines[1].contains("prefill 1200/8000"),
        "footer shows prefill: {}",
        lines[1]
    );
    assert_eq!(activity_text(&st).as_deref(), Some("Prefill 1200/8000 (3s)"));
}

/// fields
#[test]
fn tok_s_and_mode_shown() {
    let st = StatusModel {
        tok_per_s: Some(42.37),
        mode: Some(Mode::Plan),
        ..base_status()
    };
    let lines = render_footer(&vm_with(st), 120, &Theme::plain());
    assert!(lines[1].contains("42.4 tok/s"), "decode speed: {}", lines[1]);
    assert!(lines[1].contains("plan"), "permission mode: {}", lines[1]);

    let no_mode = StatusModel {
        tok_per_s: None,
        mode: None,
        ..base_status()
    };
    let lines = render_footer(&vm_with(no_mode), 120, &Theme::plain());
    assert!(
        !lines[1].contains("tok/s") && !lines[1].contains("plan"),
        "absent fields are not shown: {}",
        lines[1]
    );

    // The mode is coloured by its token.
    let st = StatusModel {
        mode: Some(Mode::Plan),
        ..base_status()
    };
    let lines = render_footer(&vm_with(st), 120, &coloured());
    assert!(
        lines[1].contains("\x1b[38;5;3m"),
        "modePlan colour applied: {}",
        lines[1]
    );
}

/// level -> token
#[test]
fn thinking_border_colour() {
    let high = StatusModel {
        thinking: Some(ThinkingLevel::High),
        ..base_status()
    };
    assert_eq!(editor_border_token(&high, ""), "thinkingHigh");
    assert_eq!(editor_border_token(&high, "hello"), "thinkingHigh");
    let off = StatusModel {
        thinking: Some(ThinkingLevel::Off),
        ..base_status()
    };
    assert_eq!(editor_border_token(&off, ""), "thinkingOff");
    let max = StatusModel {
        thinking: Some(ThinkingLevel::Max),
        ..base_status()
    };
    assert_eq!(editor_border_token(&max, ""), "thinkingMax");
    let none = StatusModel {
        thinking: None,
        ..base_status()
    };
    assert_eq!(editor_border_token(&none, ""), "border");
    // Bash mode wins over the thinking level.
    assert_eq!(editor_border_token(&high, "!ls -la"), "bashMode");
    assert_eq!(editor_border_token(&high, "!!ls"), "bashMode");
}

/// quietStartup hides the header
#[test]
fn quiet_startup() {
    let info = HeaderInfo {
        version: "0.1.0".to_string(),
        resources: vec![("skills".to_string(), vec!["review".to_string()])],
        engine_url: "http://127.0.0.1:8080".to_string(),
        model: "qwen".to_string(),
        model_remote: false,
        model_provider: None,

    };
    let keymap = Keymap::defaults();
    let vm = ViewModel::default();
    let quiet = render_header(&info, &vm, &keymap, true, 80, &Theme::plain());
    assert!(quiet.is_empty(), "quietStartup: no header");
    let loud = render_header(&info, &vm, &keymap, false, 80, &Theme::plain());
    assert!(
        loud.iter().any(|l| l.contains("oxa v0.1.0")),
        "logo line: {loud:?}"
    );
    // pi showLoadedResources: `[Name]` on its own line, the compact list indented below it.
    let at = loud.iter().position(|l| l == "[skills]").expect("section line");
    assert_eq!(loud[at + 1], "  review", "resources: {loud:?}");
    assert!(
        loud.iter().any(|l| l.contains("interrupt")),
        "key hints: {loud:?}"
    );
    for line in &loud {
        assert!(oxa_tui::visible_width(line) <= 80, "fits 80 columns: {line:?}");
    }

    let over = ViewModel {
        header_override: Some(vec!["custom".to_string()]),
        ..ViewModel::default()
    };
    assert_eq!(
        render_header(&info, &over, &keymap, false, 80, &Theme::plain()),
        vec!["custom".to_string()]
    );
}

/// sequences
#[test]
fn title_and_progress_osc() {
    let named = ViewModel {
        status: StatusModel {
            session_name: Some("fix bug".to_string()),
            ..base_status()
        },
        ..ViewModel::default()
    };
    assert_eq!(title_text(&named), "oxa - fix bug");
    let unnamed = vm_with(base_status());
    assert_eq!(title_text(&unnamed), "oxa - proj");
    let overridden = ViewModel {
        title_override: Some("ext title".to_string()),
        ..named.clone()
    };
    assert_eq!(title_text(&overridden), "ext title");
}

/// set_status keys sorted
#[test]
fn status_entries_from_extensions() {
    let mut st = base_status();
    st.ext_status.insert("zeta".to_string(), "Z-status".to_string());
    st.ext_status
        .insert("alpha".to_string(), "a\tline\nbreak".to_string());
    let lines = render_footer(&vm_with(st), 120, &Theme::plain());
    assert_eq!(lines.len(), 3, "one extra line for extension statuses");
    assert_eq!(
        lines[2], "a line break Z-status",
        "sorted by key, control characters flattened"
    );

    // Narrow width: every line fits.
    let mut st = base_status();
    st.ext_status
        .insert("alpha".to_string(), "a very long status text indeed".to_string());
    let lines = render_footer(&vm_with(st), 12, &Theme::plain());
    for line in &lines {
        assert!(oxa_tui::visible_width(line) <= 12, "fits 12 columns: {line:?}");
    }
}

// ---- additional tests -----------------------------------------------------------------------

#[test]
fn footer_override_replaces_everything() {
    let vm = ViewModel {
        footer_override: Some(vec!["only".to_string()]),
        ..vm_with(base_status())
    };
    assert_eq!(render_footer(&vm, 80, &Theme::plain()), vec!["only".to_string()]);
}

#[test]
fn footer_context_warning_colour_and_fit() {
    let st = StatusModel {
        context_tokens: 7600,
        n_ctx: 8000,
        auto_compact: true,
        ..base_status()
    };
    let lines = render_footer(&vm_with(st.clone()), 60, &coloured());
    assert!(lines[1].contains("95.0%/8.0k"), "{}", lines[1]);
    assert!(
        lines[1].contains("\x1b[38;5;1m"),
        "over 90% uses the error token: {}",
        lines[1]
    );
    assert!(lines[1].contains("(auto)"), "auto-compact marker: {}", lines[1]);
    for line in render_footer(&vm_with(st), 30, &Theme::plain()) {
        assert!(oxa_tui::visible_width(&line) <= 30, "fits 30 columns: {line:?}");
    }
}

#[test]
fn activity_text_states() {
    let mut st = base_status();
    assert_eq!(activity_text(&st), None);
    st.activity = Activity::Thinking;
    assert_eq!(activity_text(&st).as_deref(), Some("Working..."));
    st.working_message = Some("Crunching".to_string());
    assert_eq!(activity_text(&st).as_deref(), Some("Crunching"));
    st.activity = Activity::Retrying {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 1500,
    };
    assert_eq!(activity_text(&st).as_deref(), Some("Retrying in 2s (1/3)"));
    st.activity = Activity::Compacting;
    assert_eq!(activity_text(&st).as_deref(), Some("Compacting..."));
}

#[test]
fn key_hints_from_keymap() {
    let keymap = Keymap::defaults();
    let line = key_hints(
        &[("app.interrupt", "interrupt"), ("app.nonexistent", "nope")],
        &keymap,
        80,
        &Theme::plain(),
    );
    assert!(line.contains("interrupt"), "{line}");
    assert!(!line.contains("nope"), "unbound actions are skipped: {line}");
    let narrow = key_hints(
        &[("app.interrupt", "interrupt"), ("app.clear", "clear")],
        &keymap,
        10,
        &Theme::plain(),
    );
    assert!(oxa_tui::visible_width(&narrow) <= 10, "{narrow}");
}

/// Wave H (TUI-018): the start-up listing of pi's `showLoadedResources` (compact form).
#[test]
fn loaded_resources_listing() {
    use oxa_interactive::loaded_resources;
    use oxa_resources::{ContextFile, Resources};
    use std::path::PathBuf;
    let cwd = PathBuf::from("/work/proj");
    let home = PathBuf::from("/home/u");
    let res = Resources {
        context_files: vec![
            ContextFile {
                path: home.join(".oxa/agent/AGENTS.md"),
                content: String::new(),
            },
            ContextFile {
                path: cwd.join("AGENTS.md"),
                content: String::new(),
            },
        ],
        extension_paths: vec![
            PathBuf::from("/x/ext/zeta.js"),
            PathBuf::from("/x/ext/alpha/index.ts"),
        ],
        ..Resources::default()
    };
    let got = loaded_resources(&res, &cwd, &home);
    assert_eq!(
        got,
        vec![
            (
                "Context".to_string(),
                vec!["~/.oxa/agent/AGENTS.md".to_string(), "AGENTS.md".to_string()]
            ),
            (
                "Extensions".to_string(),
                vec!["alpha".to_string(), "zeta.js".to_string()]
            ),
        ]
    );
}

/// Wave D D4b: the status bar names where the model runs, both states.
#[test]
fn footer_and_header_show_the_model_location() {
    let local = render_footer(&vm_with(base_status()), 120, &Theme::plain());
    assert!(local[1].contains("local · qwen"), "local segment: {}", local[1]);

    let remote_status = StatusModel {
        model_remote: true,
        model_provider: Some("openrouter"),
        ..base_status()
    };
    let remote = render_footer(&vm_with(remote_status), 120, &Theme::plain());
    assert!(
        remote[1].contains("cloud · openrouter · qwen"),
        "cloud segment: {}",
        remote[1]
    );

    let mut theme = coloured();
    theme.colors.insert("success".to_string(), Color::Indexed(2));
    theme.colors.insert("accent".to_string(), Color::Indexed(6));
    let local_colour = render_footer(&vm_with(base_status()), 120, &theme)[1].clone();
    assert!(
        local_colour.contains(&theme.fg("success", "local")),
        "local is green: {local_colour}"
    );
    let cloud_colour = render_footer(
        &vm_with(StatusModel {
            model_remote: true,
            model_provider: Some("anthropic"),
            ..base_status()
        }),
        120,
        &theme,
    )[1]
    .clone();
    assert!(
        cloud_colour.contains(&theme.fg("accent", "cloud · anthropic")),
        "cloud is its own colour: {cloud_colour}"
    );
    assert_ne!(theme.fg("success", "location"), theme.fg("accent", "location"));
    assert!(cloud_colour.contains("cloud · anthropic"), "{cloud_colour}");

    let keymap = Keymap::defaults();
    let vm = ViewModel::default();
    let mut info = HeaderInfo {
        version: "0.2.0".to_string(),
        resources: Vec::new(),
        engine_url: "http://127.0.0.1:8092".to_string(),
        model: "qwen".to_string(),
        model_remote: false,
        model_provider: None,

    };
    let local_header = render_header(&info, &vm, &keymap, false, 100, &Theme::plain()).join("\n");
    assert!(local_header.contains("local"), "{local_header}");
    info.model_remote = true;
    let generic = render_header(&info, &vm, &keymap, false, 100, &Theme::plain()).join("\n");
    assert!(generic.contains("cloud · remote"), "{generic}");
    info.model_provider = Some("openrouter");
    let cloud_header = render_header(&info, &vm, &keymap, false, 100, &Theme::plain()).join("\n");
    assert!(cloud_header.contains("cloud · openrouter"), "{cloud_header}");
    info.model_provider = Some("openai");
    let openai_header = render_header(&info, &vm, &keymap, false, 100, &Theme::plain()).join("\n");
    assert!(openai_header.contains("cloud · openai"), "{openai_header}");
    let openai_footer = render_footer(
        &vm_with(StatusModel {
            model_remote: true,
            model_provider: Some("openai"),
            ..base_status()
        }),
        120,
        &theme,
    )[1]
    .clone();
    assert!(openai_footer.contains("cloud · openai"), "{openai_footer}");
    assert!(
        openai_footer.contains(&theme.fg("accent", "cloud · openai")),
        "{openai_footer}"
    );
}

/// Issue #51: Claude 5.5 cannot disable thinking; the footer shows what oxc sends upstream.
#[test]
fn footer_claude_55_never_shows_off() {
    let footer = |model: &str, level| {
        let st = StatusModel {
            model: model.to_string(),
            thinking: Some(level),
            ..base_status()
        };
        render_footer(&vm_with(st), 120, &Theme::plain())[1].clone()
    };
    let off = footer("anthropic/claude-sonnet-5-5", ThinkingLevel::Off);
    assert!(off.contains("adaptive • low") && !off.contains("off"), "{off}");
    let high = footer("anthropic/claude-sonnet-5-5", ThinkingLevel::High);
    assert!(high.contains("adaptive • high"), "{high}");
    assert!(footer("qwen", ThinkingLevel::Off).contains("qwen • off"));
}

/// Claude 5.5 footer over every level, and an ordinary model unchanged.
#[test]
fn footer_thinking_table_claude_55_and_ordinary() {
    let footer = |model: &str, level| {
        let st = StatusModel {
            model: model.to_string(),
            thinking: Some(level),
            ..base_status()
        };
        render_footer(&vm_with(st), 120, &Theme::plain())[1].clone()
    };
    for level in ThinkingLevel::ALL {
        let want = if level == ThinkingLevel::Off {
            "adaptive • low"
        } else {
            "adaptive • high"
        };
        let line = footer("anthropic/claude-haiku-5-5", level);
        assert!(line.contains(want), "5.5 {level:?}: {line}");
        let plain = footer("qwen", level);
        assert!(plain.contains(&format!("qwen • {}", level.as_str())), "{plain}");
    }
}
