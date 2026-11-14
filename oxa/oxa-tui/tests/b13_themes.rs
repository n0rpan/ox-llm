//! B13 acceptance tests: Themes. Owner: task B13.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_fake::TestEnv;
use oxa_proto::codes;
use oxa_tui::{
    appearance_from_colorfgbg, builtin_theme, css_color, list_themes, load_theme_file, parse_osc_color,
    parse_theme, resolve_theme, rgb_to_256, system_theme, theme_file_changed, Appearance, Color, ColorMode,
    TerminalColors, Theme, THEME_DARK_JSON, THEME_LIGHT_JSON, THEME_TOKENS,
};
use serde_json::{json, Map, Value};

/// A theme document with every required token set to the terminal default, then `overrides`
/// applied on top and `vars` as given.
fn theme_json(vars: Value, overrides: &[(&str, Value)]) -> String {
    let mut colors = Map::new();
    for (name, _, fallback) in THEME_TOKENS {
        if fallback.is_none() {
            colors.insert(name.to_string(), json!(""));
        }
    }
    for (name, value) in overrides {
        colors.insert(name.to_string(), value.clone());
    }
    json!({ "name": "test", "vars": vars, "colors": colors }).to_string()
}

fn load(json: &str) -> Theme {
    parse_theme(json, ColorMode::TrueColor).expect("theme loads")
}

/// both built-in JSON files
#[test]
fn pi_dark_and_light_load_unchanged() {
    let dark = parse_theme(THEME_DARK_JSON, ColorMode::TrueColor).expect("dark.json loads");
    let light = parse_theme(THEME_LIGHT_JSON, ColorMode::TrueColor).expect("light.json loads");
    assert_eq!(dark.name, "dark");
    assert_eq!(light.name, "light");
    assert_eq!(dark.appearance, Appearance::Dark);
    assert_eq!(light.appearance, Appearance::Light);
    // okhsl() values and var references resolve to concrete colours.
    assert!(
        matches!(dark.color("accent"), Color::Rgb(..)),
        "accent is violet via vars"
    );
    assert!(matches!(light.color("text"), Color::Rgb(..)));
    // Export colours are copied as written.
    assert_eq!(
        dark.export.get("pageBg").map(String::as_str),
        Some("okhsl(262 14% 16%)")
    );
    // The file on disk loads to the same colours and records its source.
    let env = TestEnv::new();
    let path = env.write("themes/dark.json", THEME_DARK_JSON);
    let from_file = load_theme_file(&path, ColorMode::TrueColor).expect("file loads");
    assert_eq!(from_file.colors, dark.colors);
    assert_eq!(from_file.source.as_deref(), Some(path.as_path()));
    // Built-in names resolve; the system theme needs no file.
    assert!(builtin_theme("dark", ColorMode::TrueColor, None).is_some());
    assert!(builtin_theme("light", ColorMode::TrueColor, None).is_some());
    assert!(builtin_theme("system", ColorMode::TrueColor, None).is_some());
    assert!(builtin_theme("midnight", ColorMode::TrueColor, None).is_none());
}

#[test]
fn okhsl_pins_black_and_white() {
    let t = load(&theme_json(
        json!({}),
        &[
            ("accent", json!("okhsl(0 0% 0%)")),
            ("text", json!("okhsl(0 0% 100%)")),
        ],
    ));
    assert_eq!(t.color("accent"), Color::Rgb(0, 0, 0));
    assert_eq!(t.color("text"), Color::Rgb(255, 255, 255));
}

/// variable references
#[test]
fn vars_resolve() {
    let t = load(&theme_json(
        json!({ "a": "#112233", "b": "a", "c": "b" }),
        &[("accent", json!("c")), ("border", json!(200))],
    ));
    assert_eq!(t.color("accent"), Color::Rgb(0x11, 0x22, 0x33));
    assert_eq!(t.color("border"), Color::Indexed(200));

    let cyclic = theme_json(json!({ "x": "y", "y": "x" }), &[("accent", json!("x"))]);
    let err = parse_theme(&cyclic, ColorMode::TrueColor).unwrap_err();
    assert_eq!(err.code, codes::INVALID);
    assert!(
        err.message.contains("accent") && err.message.contains("circular"),
        "{}",
        err.message
    );

    let unknown = theme_json(json!({}), &[("accent", json!("nope"))]);
    let err = parse_theme(&unknown, ColorMode::TrueColor).unwrap_err();
    assert_eq!(err.code, codes::INVALID);
    assert!(err.message.contains("nope"), "{}", err.message);
}

/// truecolor -> 256
#[test]
fn fallback_256_colour() {
    // Fixture table: cube corners, grey ramp, and the nearest-level ties of pi's algorithm.
    let table: [((u8, u8, u8), u8); 6] = [
        ((255, 0, 0), 196),
        ((0, 0, 0), 16),
        ((255, 255, 255), 231),
        ((128, 128, 128), 244),
        ((135, 135, 135), 102),
        ((95, 95, 95), 59),
    ];
    for ((r, g, b), want) in table {
        assert_eq!(rgb_to_256(r, g, b), want, "rgb_to_256({r},{g},{b})");
    }

    let json = theme_json(json!({}), &[("accent", json!("#ff0000"))]);
    let truecolor = parse_theme(&json, ColorMode::TrueColor).unwrap();
    assert_eq!(truecolor.fg_code("accent"), "\x1b[38;2;255;0;0m");
    let ansi256 = parse_theme(&json, ColorMode::Ansi256).unwrap();
    assert_eq!(ansi256.fg_code("accent"), "\x1b[38;5;196m");
}

/// light/dark detection from replies
#[test]
fn system_theme_from_osc_10_11() {
    // OSC 11 replies in the forms terminals send.
    assert_eq!(parse_osc_color("rgb:ffff/ffff/ffff"), Some((255, 255, 255)));
    assert_eq!(parse_osc_color("rgb:0000/0000/0000"), Some((0, 0, 0)));
    assert_eq!(parse_osc_color("rgb:80/80/80"), Some((128, 128, 128)));
    assert_eq!(parse_osc_color("rgba:1111/2222/3333/ffff"), Some((17, 34, 51)));
    assert_eq!(parse_osc_color("#1e1e2e"), Some((30, 30, 46)));
    assert_eq!(parse_osc_color("rgb:zz/0/0"), None);
    assert_eq!(parse_osc_color("#12345"), None);

    let light = TerminalColors {
        fg: None,
        bg: parse_osc_color("rgb:ffff/ffff/ffff"),
    };
    let dark = TerminalColors {
        fg: None,
        bg: parse_osc_color("rgb:1e1e/1e1e/2e2e"),
    };
    assert_eq!(light.appearance(), Some(Appearance::Light));
    assert_eq!(dark.appearance(), Some(Appearance::Dark));
    assert_eq!(TerminalColors::default().appearance(), None);

    let light_theme = system_theme(&light, ColorMode::TrueColor);
    assert_eq!(light_theme.name, "system");
    assert_eq!(light_theme.appearance, Appearance::Light);
    // Light terminals: panels are darker than the background, text is the terminal default.
    assert_eq!(light_theme.color("selectedBg"), Color::Rgb(230, 230, 230));
    assert_eq!(light_theme.color("text"), Color::Default);

    let dark_theme = system_theme(&dark, ColorMode::TrueColor);
    assert_eq!(dark_theme.appearance, Appearance::Dark);
    match dark_theme.color("selectedBg") {
        Color::Rgb(r, _, _) => assert!(r > 30, "dark panels are lighter than the background"),
        other => panic!("expected an RGB panel, got {other:?}"),
    }

    // Without a reported background the system theme assumes dark and tints nothing.
    let unknown = system_theme(&TerminalColors::default(), ColorMode::TrueColor);
    assert_eq!(unknown.appearance, Appearance::Dark);
    assert_eq!(unknown.color("selectedBg"), Color::Default);

    assert_eq!(appearance_from_colorfgbg(Some("15;0")), Some(Appearance::Dark));
    assert_eq!(appearance_from_colorfgbg(Some("0;15")), Some(Appearance::Light));
    assert_eq!(appearance_from_colorfgbg(Some("0;7")), Some(Appearance::Light));
    assert_eq!(appearance_from_colorfgbg(Some("default;default")), None);
    assert_eq!(appearance_from_colorfgbg(None), None);
}

/// missing token falls back
#[test]
fn invalid_theme_diagnostic() {
    // A missing token with a fallback resolves through it.
    let mut colors = Map::new();
    for (name, _, fallback) in THEME_TOKENS {
        if fallback.is_none() {
            colors.insert(name.to_string(), json!(""));
        }
    }
    colors.insert("muted".to_string(), json!("#010203"));
    colors.remove("scrollbarTrack");
    let doc = json!({ "name": "t", "colors": colors }).to_string();
    let t = load(&doc);
    assert_eq!(t.color("scrollbarTrack"), Color::Rgb(1, 2, 3));

    // A missing required token is an invalid theme that names the token.
    let err = parse_theme(r#"{"name":"t","colors":{"text":""}}"#, ColorMode::TrueColor).unwrap_err();
    assert_eq!(err.code, codes::INVALID);
    assert!(err.message.contains("accent"), "{}", err.message);

    // Malformed JSON, a missing colors map, a bad colour and a bad appearance are all invalid.
    for bad in [
        "{not json".to_string(),
        r#"{"name":"t"}"#.to_string(),
        theme_json(json!({}), &[("accent", json!("#12"))]),
        theme_json(json!({}), &[("accent", json!(300))]),
        theme_json(json!({}), &[("accent", json!("okhsl(1 2)"))]),
    ] {
        let err = parse_theme(&bad, ColorMode::TrueColor).unwrap_err();
        assert_eq!(err.code, codes::INVALID, "{bad}");
    }
    let bad_appearance = json!({ "appearance": "blue", "colors": {} }).to_string();
    assert_eq!(
        parse_theme(&bad_appearance, ColorMode::TrueColor)
            .unwrap_err()
            .code,
        codes::INVALID
    );

    let env = TestEnv::new();
    let missing = env.root().join("nope.json");
    assert_eq!(
        load_theme_file(&missing, ColorMode::TrueColor).unwrap_err().code,
        codes::IO
    );
}

/// SGR output
#[test]
fn fg_bg_apply_and_reset() {
    let t = load(&theme_json(
        json!({}),
        &[
            ("accent", json!("#ff0000")),
            ("selectedBg", json!("#101010")),
            ("border", json!(5)),
        ],
    ));
    assert_eq!(t.fg("accent", "hi"), "\x1b[38;2;255;0;0mhi\x1b[39m");
    assert_eq!(t.bg("selectedBg", "x"), "\x1b[48;2;16;16;16mx\x1b[49m");
    assert_eq!(t.fg("border", "hi"), "\x1b[38;5;5mhi\x1b[39m");
    // The terminal default emits no colour codes at all.
    assert_eq!(t.fg("text", "hi"), "hi");
    assert_eq!(t.bg("userMessageBg", "hi"), "hi");
    // Unknown tokens are the default colour, never a panic.
    assert_eq!(t.fg("noSuchToken", "hi"), "hi");
    assert_eq!(t.bold("b"), "\x1b[1mb\x1b[22m");

    assert_eq!(Theme::plain().fg("accent", "x"), "x");
    assert_eq!(Theme::plain().bold("x"), "x");
}

#[test]
fn css_colours_for_export() {
    assert_eq!(css_color(Color::Default), None);
    assert_eq!(css_color(Color::Rgb(255, 0, 16)).as_deref(), Some("#ff0010"));
    assert_eq!(css_color(Color::Indexed(196)).as_deref(), Some("#ff0000"));
    assert_eq!(css_color(Color::Indexed(232)).as_deref(), Some("#080808"));
}

#[test]
fn themes_on_disk_are_listed_and_resolved() {
    let env = TestEnv::new();
    let dir = env.root().join("themes");
    env.write(
        "themes/a.json",
        &theme_json(json!({}), &[]).replace("\"test\"", "\"zeta\""),
    );
    env.write(
        "themes/b.json",
        &theme_json(json!({}), &[]).replace("\"test\"", "\"alpha\""),
    );
    env.write("themes/c.json", "garbage");
    env.write("themes/notes.txt", "ignored");

    let listed = list_themes(std::slice::from_ref(&dir));
    let names: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["dark", "light", "system", "alpha", "zeta"]);
    assert!(listed[0].path.is_none());
    assert!(listed[4].path.is_some());

    let zeta = resolve_theme("zeta", std::slice::from_ref(&dir), ColorMode::TrueColor, None).unwrap();
    assert_eq!(zeta.name, "zeta");
    assert_eq!(zeta.source.as_deref(), Some(dir.join("a.json").as_path()));

    let missing = resolve_theme("nope", std::slice::from_ref(&dir), ColorMode::TrueColor, None).unwrap_err();
    assert_eq!(missing.code, codes::NOT_FOUND);

    let light_term = TerminalColors {
        fg: None,
        bg: Some((255, 255, 255)),
    };
    let dark_term = TerminalColors {
        fg: None,
        bg: Some((0, 0, 0)),
    };
    assert_eq!(
        resolve_theme("light/dark", &[], ColorMode::TrueColor, Some(&light_term))
            .unwrap()
            .name,
        "light"
    );
    assert_eq!(
        resolve_theme("light/dark", &[], ColorMode::TrueColor, Some(&dark_term))
            .unwrap()
            .name,
        "dark"
    );
    assert_eq!(
        resolve_theme("system", &[], ColorMode::TrueColor, Some(&light_term))
            .unwrap()
            .appearance,
        Appearance::Light
    );
}

#[test]
fn theme_file_changes_are_detected() {
    let env = TestEnv::new();
    let path = env.write("themes/live.json", &theme_json(json!({}), &[]));
    let theme = load_theme_file(&path, ColorMode::TrueColor).unwrap();
    assert!(theme_file_changed(&theme, 0));
    assert!(!theme_file_changed(&theme, u64::MAX / 2));
    let builtin = builtin_theme("dark", ColorMode::TrueColor, None).unwrap();
    assert!(!theme_file_changed(&builtin, 0));
}

#[test]
fn rgb_to_256_is_total() {
    // Every 8-bit triple maps into the palette and the mapping is stable.
    for r in (0..=255).step_by(17) {
        for g in (0..=255).step_by(17) {
            for b in (0..=255).step_by(17) {
                let n = rgb_to_256(r, g, b);
                assert_eq!(rgb_to_256(r, g, b), n);
                assert!(n >= 16);
            }
        }
    }
}
