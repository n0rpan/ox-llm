//! U2: the chrome of the OXA skin (splash, HUD, hint bar, activity line, title).
//! Spec: OXA-UI-DESIGN.md sec. 4 and 6. Public API only.
use oxa_config::Keymap;
use oxa_interactive::{
    oxa_activity_line, oxa_footer, oxa_hint_bar, oxa_hud, oxa_splash, oxa_title_text, Activity, HeaderInfo,
    HintContext, Skin, StatusModel, ViewModel,
};
use oxa_proto::{Mode, ThinkingLevel};
use oxa_tui::{strip_ansi, visible_width, ColorMode, Theme};

/// A coloured theme (the `oxa` palette may still be plain while U1 lands).
fn colour() -> Theme {
    oxa_tui::builtin_theme("dark", ColorMode::TrueColor, None).expect("builtin dark theme")
}

/// A status with every HUD segment set.
fn full_status() -> StatusModel {
    StatusModel {
        cwd: "/home/u/proj/rangekit".to_string(),
        home: "/home/u".to_string(),
        git_branch: Some("main".to_string()),
        session_name: Some("demo".to_string()),
        model: "qwen3-coder-30b".to_string(),
        thinking: Some(ThinkingLevel::Medium),
        mode: Some(Mode::Workspace),
        context_tokens: 15_000,
        n_ctx: 33_000,
        last_cached: Some(14_737),
        last_prompt: Some(14_852),
        tok_per_s: Some(42.1),
        total_in: 70_000,
        total_out: 1_600,
        ..StatusModel::default()
    }
}

fn hud(st: &StatusModel, width: u16, theme: &Theme) -> String {
    oxa_hud(st, width, theme, Skin::Oxa)
}

fn plain_hud(st: &StatusModel, width: u16) -> String {
    hud(st, width, &Theme::plain())
}

fn info() -> HeaderInfo {
    HeaderInfo {
        version: "0.2.0".to_string(),
        resources: vec![
            (
                "skills".to_string(),
                vec!["omarchy".to_string(), "ox-llm".to_string()],
            ),
            ("context".to_string(), vec!["AGENTS.md".to_string()]),
        ],
        engine_url: "http://127.0.0.1:8090".to_string(),
        model: "qwen3-coder-30b".to_string(),
        model_remote: false,
        model_provider: None,
    }
}

fn keymap() -> Keymap {
    Keymap::defaults()
}

/// Every activity variant, for the loops below.
fn all_activities() -> Vec<Activity> {
    vec![
        Activity::Prefill {
            done: 3,
            total: 8,
            secs: 2,
        },
        Activity::Thinking,
        Activity::Streaming,
        Activity::Tool("bash".to_string()),
        Activity::Compacting,
        Activity::Retrying {
            attempt: 2,
            max_attempts: 3,
            delay_ms: 1500,
        },
        Activity::Blocked,
        Activity::Queued {
            pos: Some(2),
            wait_s: 9,
        },
    ]
}

fn all_contexts() -> Vec<HintContext> {
    vec![
        HintContext::Idle {
            has_text: false,
            slash: false,
            armed: false,
        },
        HintContext::Idle {
            has_text: true,
            slash: false,
            armed: false,
        },
        HintContext::Idle {
            has_text: true,
            slash: true,
            armed: false,
        },
        HintContext::Running,
        HintContext::Dialog,
        HintContext::Overlay,
        HintContext::Search,
    ]
}

fn assert_one_line(text: &str, width: u16) {
    assert!(!text.contains('\n'), "one line only: {text:?}");
    assert!(
        visible_width(text) <= usize::from(width),
        "width {width}: {} columns in {text:?}",
        visible_width(text)
    );
}

// ---- HUD --------------------------------------------------------------------------------------

#[test]
fn hud_is_one_line_that_fits_every_width() {
    let theme = colour();
    let st = full_status();
    for width in 20..=200u16 {
        let line = hud(&st, width, &theme);
        assert_one_line(&line, width);
        let plain = plain_hud(&st, width);
        assert_one_line(&plain, width);
    }
}

#[test]
fn hud_snapshot_at_121_columns() {
    let st = full_status();
    let line = strip_ansi(&hud(&st, 121, &colour()));
    // The full line (121 columns with the location segment) fits: no segment is dropped.
    let expected = " OXA │ qwen3-coder-30b · medium │ CTX █████░░░░░ 45% 15k/33k │ CACHE █████ 99% │ 42.1 tok/s │ ● READY │ workspace │ local";
    assert_eq!(line, expected);
}

/// #50: the OXA status bar names where the model runs (D4b), cloud with its provider.
#[test]
fn hud_shows_the_model_location() {
    let remote = StatusModel {
        model_remote: true,
        model_provider: Some("anthropic"),
        ..full_status()
    };
    let line = plain_hud(&remote, 160);
    assert!(line.ends_with("workspace │ cloud · anthropic"), "{line:?}");
    assert!(!line.contains("local"), "{line:?}");
    let generic = StatusModel {
        model_provider: None,
        ..remote
    };
    assert!(plain_hud(&generic, 160).contains("cloud · remote"));
}

#[test]
fn hud_drop_order_keeps_title_and_state_down_to_40() {
    let st = full_status();
    for width in 40..=200u16 {
        let line = plain_hud(&st, width);
        assert!(line.contains("OXA"), "title at {width}: {line:?}");
        assert!(line.contains("READY"), "state at {width}: {line:?}");
        assert!(line.contains("CTX"), "context at {width}: {line:?}");
    }
}

#[test]
fn hud_segments_appear_monotonically_as_width_grows() {
    // A segment that shows at some width shows at every wider width (the drop order is fixed).
    let st = full_status();
    let markers = ["tok/s", "CACHE", "15k/33k", "medium", "workspace"];
    for marker in markers {
        let mut seen = false;
        for width in 20..=200u16 {
            let present = plain_hud(&st, width).contains(marker);
            assert!(!seen || present, "{marker} disappears again at width {width}");
            seen |= present;
        }
        assert!(seen, "{marker} never shows");
    }
}

#[test]
fn hud_drops_speed_first() {
    let st = full_status();
    let at_120 = plain_hud(&st, 121);
    assert!(at_120.contains("tok/s"), "the full line fits in 121: {at_120:?}");
    let at_100 = plain_hud(&st, 100);
    assert!(!at_100.contains("tok/s"), "speed is the first drop: {at_100:?}");
    assert!(at_100.contains("CACHE 99%") || at_100.contains("CACHE █████ 99%"));
}

#[test]
fn hud_context_thresholds() {
    let theme = colour();
    let check = |used: u32, token: &str| {
        let st = StatusModel {
            context_tokens: used,
            n_ctx: 100,
            ..StatusModel::default()
        };
        let line = hud(&st, 200, &theme);
        assert!(
            line.contains(&theme.fg(token, &format!("{used}%"))),
            "{used}% should be {token}: {line:?}"
        );
    };
    check(69, "success");
    check(70, "warning");
    check(89, "warning");
    check(90, "error");
}

#[test]
fn hud_cache_thresholds() {
    let theme = colour();
    let check = |cached: u32, token: &str| {
        let st = StatusModel {
            last_cached: Some(cached),
            last_prompt: Some(100),
            ..StatusModel::default()
        };
        let line = hud(&st, 200, &theme);
        assert!(
            line.contains(&theme.fg(token, &format!("{cached}%"))),
            "{cached}% should be {token}: {line:?}"
        );
    };
    check(49, "error");
    check(50, "warning");
    check(89, "warning");
    check(90, "success");
}

#[test]
fn hud_unknown_context_window_shows_question_mark() {
    let st = StatusModel {
        n_ctx: 0,
        context_tokens: 500,
        last_cached: None,
        ..full_status()
    };
    let line = plain_hud(&st, 200);
    assert!(line.contains("CTX ?"), "{line:?}");
    assert!(!line.contains("%"), "no percentage without a window: {line:?}");
}

#[test]
fn hud_cache_segment_absent_without_cache_info() {
    let st = StatusModel {
        last_cached: None,
        ..full_status()
    };
    assert!(!plain_hud(&st, 200).contains("CACHE"));
    let st = StatusModel {
        last_prompt: Some(0),
        ..full_status()
    };
    assert!(!plain_hud(&st, 200).contains("CACHE"));
}

#[test]
fn hud_repairs_only_when_some() {
    let none = plain_hud(&full_status(), 200);
    assert!(!none.contains("repairs"), "{none:?}");
    let st = StatusModel {
        tool_repairs: 3,
        ..full_status()
    };
    assert!(plain_hud(&st, 200).contains("repairs 3"));
}

#[test]
fn hud_state_word_when_running() {
    let st = StatusModel {
        activity: Activity::Tool("bash".to_string()),
        activity_ms: 300,
        ..full_status()
    };
    let line = plain_hud(&st, 200);
    assert!(line.contains("TOOL"), "{line:?}");
    assert!(!line.contains("READY"));
}

#[test]
fn hud_model_name_is_truncated_last() {
    let st = StatusModel {
        model: "a-very-long-model-name-for-a-narrow-screen-0123456789".to_string(),
        ..full_status()
    };
    let line = plain_hud(&st, 60);
    assert_one_line(&line, 60);
    assert!(line.contains("..."), "{line:?}");
}

#[test]
fn hud_without_model_says_no_model() {
    let st = StatusModel {
        model: String::new(),
        ..full_status()
    };
    assert!(plain_hud(&st, 200).contains("no-model"));
}

// ---- hint bar ---------------------------------------------------------------------------------

/// The first key of `id` in the keymap, as it is drawn.
fn key(id: &str) -> String {
    keymap().keys_for(id)[0].to_string()
}

fn hints(ctx: HintContext, fullscreen: bool) -> String {
    let st = StatusModel::default();
    strip_ansi(&oxa_hint_bar(
        ctx,
        &st,
        &keymap(),
        fullscreen,
        300,
        &colour(),
        Skin::Oxa,
    ))
}

#[test]
fn hint_idle_empty_editor_shows_default_keys() {
    let text = hints(
        HintContext::Idle {
            has_text: false,
            slash: false,
            armed: false,
        },
        true,
    );
    assert!(
        text.contains(&format!("{} send", key("tui.input.submit"))),
        "{text:?}"
    );
    assert!(text.contains("/ commands"), "{text:?}");
    assert!(
        text.contains(&format!("{} models", key("app.model.select"))),
        "{text:?}"
    );
    assert!(
        text.contains(&format!("{} thinking", key("app.thinking.cycle"))),
        "{text:?}"
    );
    assert!(
        text.contains(&format!("{} mode", key("app.mode.cycle"))),
        "{text:?}"
    );
    assert!(
        text.contains(&format!("{} search", key("tui.altScreen.search"))),
        "{text:?}"
    );
    assert!(text.contains(&format!("{} exit", key("app.exit"))), "{text:?}");
    // The defaults as the spec writes them.
    assert!(text.contains("enter send"), "{text:?}");
    assert!(text.contains("ctrl+l models"), "{text:?}");
    assert!(text.contains("shift+tab thinking"), "{text:?}");
    assert!(text.contains("alt+m mode"), "{text:?}");
    assert!(text.contains("ctrl+d exit"), "{text:?}");
}

#[test]
fn hint_search_only_in_fullscreen() {
    let idle = HintContext::Idle {
        has_text: false,
        slash: false,
        armed: false,
    };
    assert!(!hints(idle, false).contains("search"));
    assert!(hints(idle, true).contains("search"));
}

#[test]
fn hint_text_contexts() {
    let newline = hints(
        HintContext::Idle {
            has_text: true,
            slash: false,
            armed: false,
        },
        false,
    );
    assert!(newline.contains("enter send"), "{newline:?}");
    assert!(newline.contains("newline"), "{newline:?}");
    assert!(newline.contains("editor"), "{newline:?}");
    let slash = hints(
        HintContext::Idle {
            has_text: true,
            slash: true,
            armed: false,
        },
        false,
    );
    assert!(slash.contains("complete"), "{slash:?}");
    assert!(slash.contains("run"), "{slash:?}");
}

#[test]
fn hint_running_shows_cancel_steer_queue_expand() {
    let text = hints(HintContext::Running, false);
    assert!(
        text.contains(&format!("{} cancel", key("app.interrupt"))),
        "{text:?}"
    );
    assert!(text.contains("escape cancel"), "{text:?}");
    assert!(text.contains("enter steer"), "{text:?}");
    assert!(text.contains("alt+enter queue"), "{text:?}");
    assert!(text.contains("ctrl+o expand"), "{text:?}");
}

#[test]
fn hint_dialog_overlay_search_contexts() {
    let dialog = hints(HintContext::Dialog, false);
    assert!(
        dialog.contains("confirm") && dialog.contains("cancel"),
        "{dialog:?}"
    );
    let overlay = hints(HintContext::Overlay, false);
    assert!(overlay.contains("up/down move"), "{overlay:?}");
    assert!(overlay.contains("select"), "{overlay:?}");
    assert!(overlay.contains("close"), "{overlay:?}");
    assert!(overlay.contains("type to filter"), "{overlay:?}");
    let search = hints(HintContext::Search, true);
    assert!(
        search.contains("next") && search.contains("previous") && search.contains("close"),
        "{search:?}"
    );
}

#[test]
fn hint_rebinding_changes_the_hint() {
    let mut km = Keymap::defaults();
    let diagnostics = km.apply(&serde_json::json!({ "app.model.select": "ctrl+y" }));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let text = strip_ansi(&oxa_hint_bar(
        HintContext::Idle {
            has_text: false,
            slash: false,
            armed: false,
        },
        &StatusModel::default(),
        &km,
        false,
        300,
        &Theme::plain(),
        Skin::Oxa,
    ));
    assert!(text.contains("ctrl+y models"), "{text:?}");
    assert!(!text.contains("ctrl+l"), "{text:?}");
}

#[test]
fn hint_unbound_action_is_left_out() {
    let mut km = Keymap::defaults();
    let diagnostics = km.apply(&serde_json::json!({ "app.thinking.cycle": [] }));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let text = strip_ansi(&oxa_hint_bar(
        HintContext::Idle {
            has_text: false,
            slash: false,
            armed: false,
        },
        &StatusModel::default(),
        &km,
        false,
        300,
        &Theme::plain(),
        Skin::Oxa,
    ));
    assert!(!text.contains("thinking"), "{text:?}");
    assert!(text.contains("mode"), "{text:?}");
}

#[test]
fn hint_place_shown_at_140_and_hidden_at_50() {
    let st = full_status();
    let wide = strip_ansi(&oxa_hint_bar(
        HintContext::Idle {
            has_text: false,
            slash: false,
            armed: false,
        },
        &st,
        &keymap(),
        false,
        140,
        &colour(),
        Skin::Oxa,
    ));
    assert_eq!(visible_width(&wide), 140);
    assert!(wide.ends_with("~/proj/rangekit (main) • demo"), "{wide:?}");
    assert!(wide.contains("enter send"), "{wide:?}");
    let narrow = strip_ansi(&oxa_hint_bar(
        HintContext::Idle {
            has_text: false,
            slash: false,
            armed: false,
        },
        &st,
        &keymap(),
        false,
        50,
        &colour(),
        Skin::Oxa,
    ));
    assert!(!narrow.contains("rangekit"), "{narrow:?}");
    assert!(narrow.contains("enter send"), "{narrow:?}");
}

#[test]
fn hint_home_shortening() {
    let st = full_status();
    let text = strip_ansi(&oxa_hint_bar(
        HintContext::Dialog,
        &st,
        &keymap(),
        false,
        200,
        &Theme::plain(),
        Skin::Oxa,
    ));
    assert!(text.contains("~/proj/rangekit"), "{text:?}");
    assert!(!text.contains("/home/u"), "{text:?}");
    let no_home = StatusModel {
        home: String::new(),
        ..full_status()
    };
    let text = strip_ansi(&oxa_hint_bar(
        HintContext::Dialog,
        &no_home,
        &keymap(),
        false,
        200,
        &Theme::plain(),
        Skin::Oxa,
    ));
    assert!(text.contains("/home/u/proj/rangekit"), "{text:?}");
}

#[test]
fn hint_bar_fits_every_width() {
    let theme = colour();
    let st = full_status();
    for ctx in all_contexts() {
        for width in 10..=200u16 {
            let line = oxa_hint_bar(ctx, &st, &keymap(), true, width, &theme, Skin::Oxa);
            assert_one_line(&line, width);
        }
    }
}

// ---- activity line ----------------------------------------------------------------------------

fn activity(activity: Activity, ms: u64) -> StatusModel {
    StatusModel {
        activity,
        activity_ms: ms,
        ..StatusModel::default()
    }
}

#[test]
fn activity_badge_words() {
    let plain = Theme::plain();
    let expected = [
        "[PREFILL]",
        "[THINKING]",
        "[WRITING]",
        "[TOOL]",
        "[COMPACTING]",
        "[RETRY]",
        "[WAITING]",
        "[QUEUED]",
    ];
    for (act, word) in all_activities().into_iter().zip(expected) {
        let st = activity(act.clone(), 4000);
        let line = oxa_activity_line(&st, &keymap(), 200, &plain, Skin::Oxa)
            .unwrap_or_else(|| panic!("{act:?} has a line"));
        assert!(line.contains(word), "{act:?}: {line:?}");
    }
}

#[test]
fn activity_idle_has_no_line() {
    let st = activity(Activity::Idle, 5000);
    assert!(oxa_activity_line(&st, &keymap(), 80, &colour(), Skin::Oxa).is_none());
}

#[test]
fn activity_details() {
    let plain = Theme::plain();
    let tool = oxa_activity_line(
        &activity(Activity::Tool("bash".to_string()), 12_000),
        &keymap(),
        200,
        &plain,
        Skin::Oxa,
    )
    .unwrap_or_default();
    assert!(tool.contains("bash"), "{tool:?}");
    assert!(tool.contains("12s"), "{tool:?}");
    let prefill = oxa_activity_line(
        &activity(
            Activity::Prefill {
                done: 3,
                total: 8,
                secs: 1,
            },
            0,
        ),
        &keymap(),
        200,
        &plain,
        Skin::Oxa,
    )
    .unwrap_or_default();
    assert!(prefill.contains("3/8"), "{prefill:?}");
    assert!(prefill.contains("███"), "{prefill:?}");
    let retry = oxa_activity_line(
        &activity(
            Activity::Retrying {
                attempt: 2,
                max_attempts: 3,
                delay_ms: 1500,
            },
            0,
        ),
        &keymap(),
        200,
        &plain,
        Skin::Oxa,
    )
    .unwrap_or_default();
    assert!(retry.contains("2/3 in 2s"), "{retry:?}");
    let queued = oxa_activity_line(
        &activity(
            Activity::Queued {
                pos: Some(2),
                wait_s: 9,
            },
            31_000,
        ),
        &keymap(),
        200,
        &plain,
        Skin::Oxa,
    )
    .unwrap_or_default();
    assert!(queued.contains("#2"), "{queued:?}");
    assert!(queued.contains("31s"), "{queued:?}");
}

#[test]
fn activity_working_message_replaces_detail() {
    let st = StatusModel {
        working_message: Some("indexing the repo".to_string()),
        ..activity(Activity::Tool("bash".to_string()), 0)
    };
    let line = oxa_activity_line(&st, &keymap(), 200, &Theme::plain(), Skin::Oxa).unwrap_or_default();
    assert!(line.contains("indexing the repo"), "{line:?}");
    assert!(!line.contains("bash"), "{line:?}");
}

#[test]
fn activity_elapsed_formatting() {
    let plain = Theme::plain();
    let line = |ms: u64| {
        oxa_activity_line(
            &activity(Activity::Thinking, ms),
            &keymap(),
            200,
            &plain,
            Skin::Oxa,
        )
        .unwrap_or_default()
    };
    assert!(
        !line(0).chars().any(|c| c.is_ascii_digit()),
        "0 ms is hidden: {:?}",
        line(0)
    );
    assert!(line(4000).contains(" 4s "), "{:?}", line(4000));
    assert!(line(75_000).contains("1m15s"), "{:?}", line(75_000));
}

#[test]
fn activity_cancel_hint_with_default_keymap() {
    let plain = Theme::plain();
    let line = oxa_activity_line(
        &activity(Activity::Thinking, 4000),
        &keymap(),
        200,
        &plain,
        Skin::Oxa,
    )
    .unwrap_or_default();
    assert!(line.contains("escape to cancel"), "{line:?}");
    let blocked = oxa_activity_line(
        &activity(Activity::Blocked, 4000),
        &keymap(),
        200,
        &plain,
        Skin::Oxa,
    )
    .unwrap_or_default();
    assert!(blocked.contains("for your answer"), "{blocked:?}");
    assert!(!blocked.contains("to cancel"), "{blocked:?}");
    let mut km = Keymap::defaults();
    let _ = km.apply(&serde_json::json!({ "app.interrupt": [] }));
    let unbound = oxa_activity_line(&activity(Activity::Thinking, 4000), &km, 200, &plain, Skin::Oxa)
        .unwrap_or_default();
    assert!(!unbound.contains("to cancel"), "{unbound:?}");
}

#[test]
fn activity_fits_every_width() {
    let theme = colour();
    for act in all_activities() {
        let st = StatusModel {
            working_message: Some("a long working message that goes on and on".to_string()),
            ..activity(act.clone(), 75_000)
        };
        for width in 10..=200u16 {
            if let Some(line) = oxa_activity_line(&st, &keymap(), width, &theme, Skin::Oxa) {
                assert_one_line(&line, width);
            }
        }
    }
}

// ---- splash -----------------------------------------------------------------------------------

#[test]
fn splash_frame_is_exactly_width_wide() {
    for width in [60u16, 80, 120, 200] {
        let lines = oxa_splash(
            &info(),
            "oxa-omarchy (tokyo-night)",
            true,
            width,
            &colour(),
            Skin::Oxa,
        );
        let framed: Vec<&String> = lines
            .iter()
            .filter(|l| {
                let plain = strip_ansi(l);
                plain.starts_with('╔') || plain.starts_with('║') || plain.starts_with('╚')
            })
            .collect();
        assert_eq!(framed.len(), 5, "top, three rows, bottom at {width}: {lines:?}");
        for line in framed {
            assert_eq!(visible_width(line), usize::from(width), "{line:?}");
        }
    }
}

#[test]
fn splash_has_logo_and_part_of_suite_at_120() {
    let lines = oxa_splash(&info(), "oxa", true, 120, &colour(), Skin::Oxa);
    let text = strip_ansi(&lines.join("\n"));
    assert!(text.contains("▄▀▀▄ ▀▄ ▄▀ ▄▀▀▄"), "{text}");
    assert!(text.contains("OXA v0.2.0"), "{text}");
    assert!(text.contains("part of ox-llm"), "{text}");
    assert!(text.contains("qwen3-coder-30b"), "{text}");
    assert!(text.contains("http://127.0.0.1:8090"), "{text}");
    assert!(text.contains("theme oxa"), "{text}");
}

#[test]
fn splash_shows_where_the_model_runs() {
    let local = strip_ansi(&oxa_splash(&info(), "oxa", true, 120, &colour(), Skin::Oxa).join("\n"));
    assert!(local.contains("local"), "local location: {local}");
    let mut remote = info();
    remote.model_remote = true;
    remote.model_provider = Some("anthropic");
    let cloud = strip_ansi(&oxa_splash(&remote, "oxa", true, 120, &colour(), Skin::Oxa).join("\n"));
    assert!(cloud.contains("cloud · anthropic"), "cloud location: {cloud}");
}

#[test]
fn splash_below_44_has_no_frame_and_no_logo() {
    let lines = oxa_splash(&info(), "oxa", true, 40, &colour(), Skin::Oxa);
    let text = strip_ansi(&lines.join("\n"));
    assert!(!text.contains('╔') && !text.contains('║'), "{text}");
    assert!(!text.contains("▄▀▀▄"), "{text}");
    assert!(text.contains("OXA v0.2.0"), "{text}");
    for line in &lines {
        assert!(visible_width(line) <= 40, "{line:?}");
    }
}

#[test]
fn splash_details_flag_controls_resources() {
    let with = strip_ansi(&oxa_splash(&info(), "oxa", true, 100, &colour(), Skin::Oxa).join("\n"));
    assert!(with.contains("AGENTS.md"), "{with}");
    assert!(with.contains("skills"), "{with}");
    let without = strip_ansi(&oxa_splash(&info(), "oxa", false, 100, &colour(), Skin::Oxa).join("\n"));
    assert!(!without.contains("AGENTS.md"), "{without}");
    assert!(without.contains('╚'), "the frame stays: {without}");
}

#[test]
fn splash_fits_every_width() {
    let theme = colour();
    for width in 1..=200u16 {
        for details in [true, false] {
            let lines = oxa_splash(
                &info(),
                "oxa-omarchy (tokyo-night)",
                details,
                width,
                &theme,
                Skin::Oxa,
            );
            for line in &lines {
                assert_one_line(line, width);
            }
        }
    }
}

// ---- footer and title -------------------------------------------------------------------------

#[test]
fn footer_has_hud_hints_and_extension_line() {
    let mut vm = ViewModel {
        status: full_status(),
        ..ViewModel::default()
    };
    let lines = oxa_footer(
        &vm,
        HintContext::Running,
        &keymap(),
        false,
        120,
        &Theme::plain(),
        Skin::Oxa,
    );
    assert_eq!(lines.len(), 2, "{lines:?}");
    vm.status
        .ext_status
        .insert("a".to_string(), "mcp: 2 up".to_string());
    let lines = oxa_footer(
        &vm,
        HintContext::Running,
        &keymap(),
        false,
        120,
        &Theme::plain(),
        Skin::Oxa,
    );
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[2].contains("mcp: 2 up"));
}

#[test]
fn footer_override_replaces_everything() {
    let vm = ViewModel {
        footer_override: Some(vec!["custom one".to_string(), "custom two".to_string()]),
        ..ViewModel::default()
    };
    let lines = oxa_footer(
        &vm,
        HintContext::Dialog,
        &keymap(),
        false,
        80,
        &Theme::plain(),
        Skin::Oxa,
    );
    assert_eq!(lines, vec!["custom one".to_string(), "custom two".to_string()]);
}

#[test]
fn title_text_rules() {
    let vm = ViewModel {
        status: StatusModel {
            session_name: Some("demo".to_string()),
            ..StatusModel::default()
        },
        ..ViewModel::default()
    };
    assert_eq!(oxa_title_text(&vm), "OXA - demo");

    let vm = ViewModel {
        status: StatusModel {
            cwd: "/home/u/proj/rangekit".to_string(),
            ..StatusModel::default()
        },
        ..ViewModel::default()
    };
    assert_eq!(oxa_title_text(&vm), "OXA - rangekit");

    let vm = ViewModel {
        status: StatusModel {
            session_name: Some("de\u{7}mo\n\u{1b}".to_string()),
            ..StatusModel::default()
        },
        ..ViewModel::default()
    };
    assert_eq!(oxa_title_text(&vm), "OXA - demo");

    let vm = ViewModel {
        title_override: Some("Custom title".to_string()),
        status: StatusModel {
            session_name: Some("demo".to_string()),
            ..StatusModel::default()
        },
        ..ViewModel::default()
    };
    assert_eq!(oxa_title_text(&vm), "Custom title");
}

// ---- plain theme and ASCII skin ---------------------------------------------------------------

/// Every chrome output for `st`, rendered with `theme` and `skin`.
fn render_all(st: &StatusModel, theme: &Theme, skin: Skin) -> Vec<String> {
    let km = keymap();
    let vm = ViewModel {
        status: st.clone(),
        ..ViewModel::default()
    };
    let mut out = Vec::new();
    out.extend(oxa_splash(
        &info(),
        "oxa-omarchy (tokyo-night)",
        true,
        100,
        theme,
        skin,
    ));
    out.extend(oxa_splash(&info(), "oxa", true, 40, theme, skin));
    out.push(oxa_hud(st, 120, theme, skin));
    out.push(oxa_hud(st, 50, theme, skin));
    for ctx in all_contexts() {
        out.push(oxa_hint_bar(ctx, st, &km, true, 120, theme, skin));
    }
    for act in all_activities() {
        let st = StatusModel {
            activity: act,
            ..st.clone()
        };
        out.extend(oxa_activity_line(&st, &km, 120, theme, skin));
    }
    out.extend(oxa_footer(
        &vm,
        HintContext::Running,
        &km,
        false,
        120,
        theme,
        skin,
    ));
    out
}

#[test]
fn plain_theme_has_no_escape_sequence() {
    let plain = Theme::plain();
    let lines = render_all(&full_status(), &plain, Skin::Oxa);
    assert!(!lines.is_empty());
    for line in &lines {
        assert!(!line.contains('\u{1b}'), "escape in {line:?}");
    }
}

#[test]
fn coloured_theme_does_carry_escape_sequences() {
    let lines = render_all(&full_status(), &colour(), Skin::Oxa);
    assert!(lines.iter().any(|l| l.contains('\u{1b}')));
}

#[test]
fn ascii_skin_is_pure_ascii() {
    let theme = colour();
    let st = StatusModel {
        cwd: "/home/u/proj/rangekit".to_string(),
        home: "/home/u".to_string(),
        git_branch: Some("main".to_string()),
        session_name: Some("demo".to_string()),
        activity: Activity::Tool("bash".to_string()),
        activity_ms: 4000,
        ..full_status()
    };
    let km = keymap();
    let vm = ViewModel {
        status: st.clone(),
        ..ViewModel::default()
    };
    let mut lines = render_all(&st, &theme, Skin::OxaAscii);
    for width in [20u16, 60, 120] {
        lines.push(oxa_hud(&st, width, &theme, Skin::OxaAscii));
        lines.push(oxa_hint_bar(
            HintContext::Overlay,
            &st,
            &km,
            true,
            width,
            &theme,
            Skin::OxaAscii,
        ));
    }
    lines.extend(oxa_footer(
        &vm,
        HintContext::Idle {
            has_text: false,
            slash: false,
            armed: false,
        },
        &km,
        true,
        120,
        &theme,
        Skin::OxaAscii,
    ));
    for line in &lines {
        assert!(line.is_ascii(), "non-ASCII glyph in {line:?}");
    }
}

#[test]
fn ascii_skin_splash_cloud_label_is_ascii() {
    let theme = Theme::plain();
    let cloud = HeaderInfo {
        model_remote: true,
        model_provider: Some("openrouter"),
        ..info()
    };
    for line in oxa_splash(&cloud, "oxa", true, 100, &theme, Skin::OxaAscii) {
        assert!(line.is_ascii(), "non-ASCII glyph in {line:?}");
    }
}

// ---- robustness -------------------------------------------------------------------------------

#[test]
fn never_panics_on_odd_content_and_fits_every_width() {
    let odd = StatusModel {
        cwd: "/home/日本/プロジェクト/😀\u{7}\n".to_string(),
        home: "/home/日本".to_string(),
        git_branch: Some("ブランチ\t".to_string()),
        session_name: Some(String::new()),
        model: "モデル😀\u{1b}[31m".to_string(),
        thinking: Some(ThinkingLevel::High),
        thinking_note: Some("forced\noff".to_string()),
        mode: Some(Mode::Plan),
        context_tokens: 500,
        n_ctx: 0,
        last_cached: Some(900),
        last_prompt: Some(100),
        tok_per_s: Some(f64::NAN),
        tool_repairs: 7,
        activity: Activity::Tool("名前\n".to_string()),
        activity_ms: u64::MAX,
        working_message: Some(String::new()),
        ..StatusModel::default()
    };
    let theme = colour();
    let km = keymap();
    let vm = ViewModel {
        status: odd.clone(),
        ..ViewModel::default()
    };
    let odd_info = HeaderInfo {
        version: String::new(),
        resources: vec![(String::new(), vec![String::new()])],
        engine_url: "\n".to_string(),
        model: String::new(),
        model_remote: false,
        model_provider: None,
    };
    for width in 1..=300u16 {
        for skin in [Skin::Oxa, Skin::OxaAscii] {
            let hud_line = oxa_hud(&odd, width, &theme, skin);
            assert_one_line(&hud_line, width);
            let hint = oxa_hint_bar(HintContext::Overlay, &odd, &km, true, width, &theme, skin);
            assert_one_line(&hint, width);
            if let Some(line) = oxa_activity_line(&odd, &km, width, &theme, skin) {
                assert_one_line(&line, width);
            }
            for line in oxa_footer(&vm, HintContext::Running, &km, false, width, &theme, skin) {
                assert_one_line(&line, width);
            }
            for line in oxa_splash(&odd_info, "\n", true, width, &theme, skin) {
                assert_one_line(&line, width);
            }
        }
    }
    // n_ctx 0 shows the unknown window; cached above the prompt is capped at 100 %.
    assert!(strip_ansi(&oxa_hud(&odd, 300, &theme, Skin::Oxa)).contains("CTX ?"));
    assert!(strip_ansi(&oxa_hud(&odd, 300, &theme, Skin::Oxa)).contains("100%"));
    let _ = oxa_title_text(&vm);
}

#[test]
fn zero_width_neither_panics_nor_overflows() {
    let st = full_status();
    let theme = colour();
    let km = keymap();
    assert_one_line(&oxa_hud(&st, 0, &theme, Skin::Oxa), 0);
    assert_one_line(
        &oxa_hint_bar(HintContext::Dialog, &st, &km, false, 0, &theme, Skin::Oxa),
        0,
    );
    assert!(oxa_splash(&info(), "oxa", true, 0, &theme, Skin::Oxa)
        .iter()
        .all(|l| visible_width(l) == 0));
}

/// Thinking label in the oxa skin HUD: Claude 5.5 shows the effective effort, others the level.
#[test]
fn hud_thinking_label_claude_55_effective_effort() {
    let hud_for = |model: &str, level| {
        let st = StatusModel {
            model: model.to_string(),
            thinking: Some(level),
            ..StatusModel::default()
        };
        strip_ansi(&plain_hud(&st, 160))
    };
    let all = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ];
    for level in all {
        let want = if level == ThinkingLevel::Off {
            "adaptive · low"
        } else {
            "adaptive · high"
        };
        let line = hud_for("anthropic/claude-haiku-5-5", level);
        assert!(line.contains(want), "5.5 {level:?}: {line}");
        assert!(!line.contains(" off"), "5.5 never shows off: {line}");
    }
    let line = hud_for("qwen-local", ThinkingLevel::High);
    assert!(line.contains("high") && !line.contains("adaptive"), "{line}");
    let line = hud_for("qwen-local", ThinkingLevel::Off);
    assert!(line.contains("off") && !line.contains("adaptive"), "{line}");
}

/// #343: at 60 columns a cloud model still shows where it runs and the permission mode.
#[test]
fn hud_keeps_location_at_60_columns() {
    let st = StatusModel {
        model: "anthropic/claude-haiku-5-5".to_string(),
        model_remote: true,
        model_provider: Some("anthropic"),
        ..full_status()
    };
    for width in 60..=70u16 {
        let line = plain_hud(&st, width);
        assert!(
            line.contains("cloud · anthropic"),
            "location at {width}: {line:?}"
        );
        if width >= 70 {
            assert!(line.contains("workspace"), "mode at {width}: {line:?}");
        }
        assert!(line.contains("READY"), "state at {width}: {line:?}");
    }
}

#[test]
fn hint_armed_shows_clear_key_as_exit() {
    let idle = |armed| HintContext::Idle {
        has_text: false,
        slash: false,
        armed,
    };
    let plain = hints(idle(false), false);
    let armed = hints(idle(true), false);
    assert!(plain.contains("ctrl+d exit"), "{plain:?}");
    assert!(armed.contains("ctrl+c exit"), "{armed:?}");
    assert!(!armed.contains("ctrl+d exit"), "{armed:?}");
}
