//! U1 acceptance tests: the OXA themes (Omarchy palette reader and palette -> theme mapping).
//! Public API only (plus std). Temp directories are unique per test and removed
//! afterwards; no Omarchy file is ever touched.

use oxa_tui::{
    load_omarchy_palette, mix_rgb, omarchy_colors_paths, oxa_builtin_palette, oxa_theme,
    parse_omarchy_colors, Appearance, Color, ColorMode, OmarchyPalette, Rgb, Theme, OMARCHY_COLORS_MAX_BYTES,
    THEME_TOKENS,
};
use std::path::{Path, PathBuf};

/// The current Omarchy generation (copied from a real `colors.toml`).
const SAMPLE_CURRENT: &str = r##"mode = "dark"

accent = "#7aa2f7"
selection = "#292e42"
muted = "#414868"

background = "#1a1b26"
dark_background = "#13141c"
darker_background = "#0e0e14"
lighter_background = "#24283b"

foreground = "#a9b1d6"
dark_foreground = "#565f89"
light_foreground = "#b4bee6"
bright_foreground = "#c0caf5"

red = "#f7768e"
yellow = "#e0af68"
orange = "#eb927b"
green = "#9ece6a"
cyan = "#449dab"
blue = "#7aa2f7"
magenta = "#ad8ee6"
brown = "#75493d"

bright_red = "#ff7a93"
bright_yellow = "#ff9e64"
bright_green = "#b9f27c"
bright_cyan = "#0db9d7"
bright_blue = "#7da6ff"
bright_magenta = "#bb9af7"
"##;

/// The older generation (key names of the Omarchy 3 `colors.toml`).
const SAMPLE_OLDER: &str = r##"accent = "#7aa2f7"
cursor = "#c0caf5"
foreground = "#a9b1d6"
background = "#1a1b26"
selection_foreground = "#c0caf5"
selection_background = "#7aa2f7"
color0 = "#32344a"
color1 = "#f7768e"
color2 = "#9ece6a"
color3 = "#e0af68"
color4 = "#7aa2f7"
color5 = "#ad8ee6"
color6 = "#449dab"
color7 = "#787c99"
color8 = "#414868"
color9 = "#f7768e"
color10 = "#9ece6a"
color11 = "#e0af68"
color12 = "#7aa2f7"
color13 = "#bb9af7"
color14 = "#0db9d7"
color15 = "#c0caf5"
"##;

/// A light palette (`mode` left out; the background decides).
const LIGHT: &str = r##"background = "#f5f5f0"
foreground = "#2a2a33"
accent = "#3b6fd6"
"##;

/// A unique scratch directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("oxa-u1-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write `text` to `path`, creating the parent directories.
fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    std::fs::write(path, text).expect("write file");
}

/// The colour of `token` in `theme`, which must be `Color::Rgb`.
fn rgb_of(theme: &Theme, token: &str) -> Rgb {
    match theme.colors.get(token) {
        Some(Color::Rgb(r, g, b)) => (*r, *g, *b),
        other => panic!("token {token} is not an Rgb colour: {other:?}"),
    }
}

/// WCAG 2.x relative luminance (sRGB gamma).
fn wcag_luminance((r, g, b): Rgb) -> f64 {
    let lin = |v: u8| {
        let c = f64::from(v) / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// WCAG contrast ratio between two colours.
fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (wcag_luminance(a), wcag_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn is_css_hex(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A palette from `text` through the public parser, or a panic.
fn palette(text: &str) -> OmarchyPalette {
    parse_omarchy_colors(text).expect("palette parses")
}

#[test]
fn current_generation_sample_parses_every_field() {
    let p = palette(SAMPLE_CURRENT);
    assert_eq!(p.name, None);
    assert!(!p.light);
    assert_eq!(p.background, (0x1a, 0x1b, 0x26));
    assert_eq!(p.lighter_background, (0x24, 0x28, 0x3b));
    assert_eq!(p.foreground, (0xa9, 0xb1, 0xd6));
    assert_eq!(p.dark_foreground, (0x56, 0x5f, 0x89));
    assert_eq!(p.bright_foreground, (0xc0, 0xca, 0xf5));
    assert_eq!(p.accent, (0x7a, 0xa2, 0xf7));
    assert_eq!(p.selection, (0x29, 0x2e, 0x42));
    assert_eq!(p.muted, (0x41, 0x48, 0x68));
    assert_eq!(p.red, (0xf7, 0x76, 0x8e));
    assert_eq!(p.green, (0x9e, 0xce, 0x6a));
    assert_eq!(p.yellow, (0xe0, 0xaf, 0x68));
    assert_eq!(p.orange, (0xeb, 0x92, 0x7b));
    assert_eq!(p.blue, (0x7a, 0xa2, 0xf7));
    assert_eq!(p.magenta, (0xad, 0x8e, 0xe6));
    assert_eq!(p.cyan, (0x44, 0x9d, 0xab));
}

#[test]
fn older_generation_uses_selection_background_and_colour_keys() {
    let p = palette(SAMPLE_OLDER);
    assert!(!p.light);
    assert_eq!(p.accent, (0x7a, 0xa2, 0xf7));
    // `selection_background` stands in for `selection`, toned down toward the background.
    assert_eq!(
        p.selection,
        oxa_tui::mix_rgb(p.background, (0x7a, 0xa2, 0xf7), 0.30)
    );
    // `color8` is the muted colour, `color15` the bright foreground.
    assert_eq!(p.muted, (0x41, 0x48, 0x68));
    assert_eq!(p.bright_foreground, (0xc0, 0xca, 0xf5));
    // `color1`..`color6` are red, green, yellow, blue, magenta, cyan.
    assert_eq!(p.red, (0xf7, 0x76, 0x8e));
    assert_eq!(p.green, (0x9e, 0xce, 0x6a));
    assert_eq!(p.yellow, (0xe0, 0xaf, 0x68));
    assert_eq!(p.blue, (0x7a, 0xa2, 0xf7));
    assert_eq!(p.magenta, (0xad, 0x8e, 0xe6));
    assert_eq!(p.cyan, (0x44, 0x9d, 0xab));
    // No `orange` and no `bright_yellow`: orange is the resolved yellow.
    assert_eq!(p.orange, p.yellow);
}

#[test]
fn derivations_from_background_and_foreground_are_exact() {
    // Only the two required keys: everything else is derived (sec. 3.1).
    let p = palette("background = \"#1a1b26\"\nforeground = \"#a9b1d6\"\n");
    let builtin = oxa_builtin_palette();
    assert!(!p.light);
    assert_eq!(p.lighter_background, (36, 38, 50));
    assert_eq!(p.selection, (47, 50, 64));
    assert_eq!(p.muted, (62, 65, 82));
    assert_eq!(p.dark_foreground, (98, 102, 126));
    assert_eq!(p.bright_foreground, (0xa9, 0xb1, 0xd6));
    // Colours the file leaves out come from the built-in oxa palette.
    assert_eq!(p.accent, builtin.blue);
    assert_eq!(p.red, builtin.red);
    assert_eq!(p.green, builtin.green);
    assert_eq!(p.yellow, builtin.yellow);
    assert_eq!(p.blue, builtin.blue);
    assert_eq!(p.magenta, builtin.magenta);
    assert_eq!(p.cyan, builtin.cyan);
    assert_eq!(p.orange, builtin.yellow);
}

#[test]
fn orange_comes_from_bright_yellow_before_yellow() {
    let p = palette("background = \"#1a1b26\"\nforeground = \"#a9b1d6\"\nbright_yellow = \"#ff9e64\"\n");
    assert_eq!(p.orange, (0xff, 0x9e, 0x64));
}

#[test]
fn mode_key_and_luminance_decide_light() {
    let dark_bg_light_mode =
        palette("mode = \"light\"\nbackground = \"#1a1b26\"\nforeground = \"#a9b1d6\"\n");
    assert!(dark_bg_light_mode.light);
    let light_bg_dark_mode = palette("mode = \"dark\"\nbackground = \"#f5f5f0\"\nforeground = \"#2a2a33\"\n");
    assert!(!light_bg_dark_mode.light);
    // No `mode`: the luminance of the background (> 0.5 is light).
    let light_bg = palette(LIGHT);
    assert!(light_bg.light);
    assert!(!palette(SAMPLE_CURRENT).light);
}

#[test]
fn three_digit_hex_and_case_and_quotes_and_comments() {
    let text = "\
# a comment
[colors]
background = '#abc'
foreground = \"#ABCDEF\"
accent = \"#fA0\" # trailing comment after the value
red = '#F00'   # single quotes
";
    let p = palette(text);
    assert_eq!(p.background, (0xaa, 0xbb, 0xcc));
    assert_eq!(p.foreground, (0xab, 0xcd, 0xef));
    assert_eq!(p.accent, (0xff, 0xaa, 0x00));
    assert_eq!(p.red, (0xff, 0x00, 0x00));
}

#[test]
fn missing_or_bad_background_or_foreground_gives_none() {
    assert!(parse_omarchy_colors("").is_none());
    assert!(parse_omarchy_colors("accent = \"#7aa2f7\"\n").is_none());
    assert!(parse_omarchy_colors("background = \"#1a1b26\"\n").is_none());
    assert!(parse_omarchy_colors("foreground = \"#a9b1d6\"\n").is_none());
    // Not #RRGGBB / #RGB, not quoted, unclosed quote.
    assert!(parse_omarchy_colors("background = \"#12345\"\nforeground = \"#fff\"\n").is_none());
    assert!(parse_omarchy_colors("background = \"#zzzzzz\"\nforeground = \"#fff\"\n").is_none());
    assert!(parse_omarchy_colors("background = #1a1b26\nforeground = \"#fff\"\n").is_none());
    assert!(parse_omarchy_colors("background = \"#1a1b26\nforeground = \"#fff\"\n").is_none());
    assert!(parse_omarchy_colors("background = \"1a1b26\"\nforeground = \"#fff\"\n").is_none());
}

#[test]
fn malformed_lines_are_ignored_and_the_last_valid_value_wins() {
    let text = "\
=
\"
'
[[
background
background =
background = \"#12\"
background = \"#101010\"
foreground = 'ffffff'
foreground = \"#fff\"
";
    let p = palette(text);
    assert_eq!(p.background, (0x10, 0x10, 0x10));
    assert_eq!(p.foreground, (0xff, 0xff, 0xff));
}

#[test]
fn junk_never_panics_and_gives_none_without_background_or_foreground() {
    let long_value = format!("background = \"{}\"", "a".repeat(1 << 20));
    let long_key = format!("{} = \"#000000\"", "k".repeat(1 << 20));
    let binary: Vec<u8> = (0..=255u8).cycle().take(8192).collect();
    let binary_text = String::from_utf8_lossy(&binary).into_owned();
    let multibyte = "background = \"\u{00e9}\u{1f600}\"\nforeground = \"\u{4e2d}\"\n";
    let inputs: Vec<&str> = vec![
        "",
        "\n\n\n",
        "junk without equals",
        "= = = = =",
        "\"\"\"\"''''",
        "[section]",
        "#",
        "background = \"\"",
        "background = \"#\"",
        "background = \"#1234567\"",
        multibyte,
        &long_value,
        &long_key,
        &binary_text,
    ];
    for text in inputs {
        assert!(
            parse_omarchy_colors(text).is_none(),
            "junk gave a palette: {:?}",
            &text[..text.len().min(40)]
        );
    }
    // Junk around valid lines: the valid lines are used.
    let mixed = format!("{binary_text}\nbackground = \"#000000\"\nforeground = \"#ffffff\"\n{long_key}\n");
    assert!(parse_omarchy_colors(&mixed).is_some());
}

#[test]
fn mix_rgb_rounds_per_channel() {
    assert_eq!(mix_rgb((0, 0, 0), (255, 255, 255), 0.5), (128, 128, 128));
    assert_eq!(mix_rgb((10, 20, 30), (200, 100, 0), 0.0), (10, 20, 30));
    assert_eq!(mix_rgb((10, 20, 30), (200, 100, 0), 1.0), (200, 100, 0));
    assert_eq!(mix_rgb((26, 27, 38), (169, 177, 214), 0.07), (36, 38, 50));
}

#[test]
fn candidate_paths_with_and_without_xdg_dirs() {
    let home = Path::new("/home/someone");
    let rel = Path::new("omarchy/current/theme/colors.toml");

    let default = omarchy_colors_paths(home, None, None);
    assert_eq!(
        default,
        vec![
            home.join(".local/state").join(rel),
            home.join(".config").join(rel)
        ]
    );

    let state = Path::new("/xdg/state");
    let config = Path::new("/xdg/config");
    let xdg = omarchy_colors_paths(home, Some(state), Some(config));
    assert_eq!(xdg, vec![state.join(rel), config.join(rel)]);

    // Only one XDG dir given: the other keeps its default.
    let half = omarchy_colors_paths(home, Some(state), None);
    assert_eq!(half, vec![state.join(rel), home.join(".config").join(rel)]);
}

#[test]
fn load_prefers_the_state_candidate_then_falls_back_to_config() {
    let tmp = TempDir::new("order");
    let state = tmp.path().join("state");
    let config = tmp.path().join("config");
    let rel = "omarchy/current/theme/colors.toml";
    let state_file = state.join(rel);
    let config_file = config.join(rel);
    write(&config_file, LIGHT);
    let candidates = vec![state_file.clone(), config_file.clone()];

    // Only the config file exists.
    let (p, path) = load_omarchy_palette(&candidates).expect("config candidate");
    assert_eq!(path, config_file);
    assert!(p.light);

    // Both exist: the state file wins.
    write(&state_file, SAMPLE_CURRENT);
    let (p, path) = load_omarchy_palette(&candidates).expect("state candidate");
    assert_eq!(path, state_file);
    assert!(!p.light);

    // The state file is junk: the next candidate is used.
    write(&state_file, "nothing useful here");
    let (_, path) = load_omarchy_palette(&candidates).expect("fallback to config");
    assert_eq!(path, config_file);

    // Nothing exists.
    let missing = vec![tmp.path().join("nope/colors.toml")];
    assert!(load_omarchy_palette(&missing).is_none());
    assert!(load_omarchy_palette(&[]).is_none());
}

#[test]
fn a_directory_named_like_the_file_is_not_a_candidate() {
    let tmp = TempDir::new("dir-candidate");
    let dir_as_file = tmp.path().join("colors.toml");
    std::fs::create_dir_all(&dir_as_file).expect("dir");
    let file = tmp.path().join("other/colors.toml");
    write(&file, SAMPLE_CURRENT);
    let (_, path) = load_omarchy_palette(&[dir_as_file, file.clone()]).expect("regular file");
    assert_eq!(path, file);
}

#[test]
fn size_cap_ignores_a_big_file_and_uses_the_next_candidate() {
    let tmp = TempDir::new("size-cap");
    let big_file = tmp.path().join("big/colors.toml");
    let small_file = tmp.path().join("small/colors.toml");

    // 70 KiB: valid colours plus a long comment line.
    let mut big = String::from(SAMPLE_CURRENT);
    big.push_str(&"#".repeat(70 * 1024));
    big.push('\n');
    assert!(big.len() > OMARCHY_COLORS_MAX_BYTES as usize);
    write(&big_file, &big);
    write(&small_file, LIGHT);

    let (p, path) = load_omarchy_palette(&[big_file.clone(), small_file.clone()]).expect("small file");
    assert_eq!(path, small_file);
    assert!(p.light);

    // Alone, the big file gives nothing.
    assert!(load_omarchy_palette(&[big_file]).is_none());
}

#[test]
fn a_file_of_exactly_the_cap_is_read() {
    let tmp = TempDir::new("exact-cap");
    let file = tmp.path().join("colors.toml");
    let mut text = String::from(SAMPLE_CURRENT);
    let pad = OMARCHY_COLORS_MAX_BYTES as usize - text.len();
    text.push_str(&"#".repeat(pad));
    assert_eq!(text.len() as u64, OMARCHY_COLORS_MAX_BYTES);
    write(&file, &text);
    assert!(load_omarchy_palette(&[file]).is_some());
}

#[test]
fn non_utf8_file_does_not_panic() {
    let tmp = TempDir::new("binary");
    let file = tmp.path().join("colors.toml");
    let mut bytes = b"background = \"#101010\"\nforeground = \"#fafafa\"\n".to_vec();
    bytes.extend([0xff, 0xfe, 0x00, 0x80, b'"', b'=', 0xc3]);
    std::fs::write(&file, &bytes).expect("write");
    let (p, _) = load_omarchy_palette(std::slice::from_ref(&file)).expect("parses lossily");
    assert_eq!(p.background, (0x10, 0x10, 0x10));
    let junk = tmp.path().join("junk/colors.toml");
    std::fs::create_dir_all(junk.parent().expect("parent")).expect("create dir");
    std::fs::write(&junk, [0xffu8; 4096]).expect("write");
    assert!(load_omarchy_palette(&[junk]).is_none());
}

#[test]
fn name_comes_from_theme_name_file() {
    let tmp = TempDir::new("name-file");
    let file = tmp.path().join("current/theme/colors.toml");
    write(&file, SAMPLE_CURRENT);
    write(
        &tmp.path().join("current/theme.name"),
        "tok\u{7}yo-night  \r\nsecond line\n",
    );
    let (p, _) = load_omarchy_palette(std::slice::from_ref(&file)).expect("palette");
    assert_eq!(p.name.as_deref(), Some("tokyo-night"));
}

#[test]
fn theme_name_file_too_long_or_empty_falls_back_to_none_for_plain_dir() {
    let tmp = TempDir::new("name-bad");
    let file = tmp.path().join("current/theme/colors.toml");
    write(&file, SAMPLE_CURRENT);
    write(
        &tmp.path().join("current/theme.name"),
        &format!("{}\n", "n".repeat(65)),
    );
    let (p, _) = load_omarchy_palette(std::slice::from_ref(&file)).expect("palette");
    assert_eq!(p.name, None);
    write(&tmp.path().join("current/theme.name"), "   \n");
    let (p, _) = load_omarchy_palette(std::slice::from_ref(&file)).expect("palette");
    assert_eq!(p.name, None);
}

#[cfg(unix)]
#[test]
fn name_comes_from_a_symlinked_theme_directory() {
    let tmp = TempDir::new("name-symlink");
    let real_dir = tmp.path().join("themes/tokyo-night");
    write(&real_dir.join("colors.toml"), SAMPLE_CURRENT);
    let current = tmp.path().join("current");
    std::fs::create_dir_all(&current).expect("current");
    std::os::unix::fs::symlink(&real_dir, current.join("theme")).expect("symlink");

    let candidate = current.join("theme/colors.toml");
    let (p, path) = load_omarchy_palette(std::slice::from_ref(&candidate)).expect("palette");
    assert_eq!(path, candidate);
    assert_eq!(p.name.as_deref(), Some("tokyo-night"));

    // A theme.name file still wins over the directory name.
    write(&current.join("theme.name"), "Named\n");
    let (p, _) = load_omarchy_palette(std::slice::from_ref(&candidate)).expect("palette");
    assert_eq!(p.name.as_deref(), Some("Named"));
}

#[test]
fn builtin_palette_and_sample_set_every_token_to_rgb() {
    let builtin = oxa_builtin_palette();
    let sample = palette(SAMPLE_CURRENT);
    for p in [&builtin, &sample] {
        let theme = oxa_theme(p, "test", ColorMode::TrueColor);
        for (token, _, _) in THEME_TOKENS {
            assert!(
                matches!(theme.colors.get(token), Some(Color::Rgb(..))),
                "token {token} is not set to an Rgb colour"
            );
            assert!(matches!(theme.color(token), Color::Rgb(..)));
        }
        assert_eq!(theme.colors.len(), THEME_TOKENS.len());
    }
}

#[test]
fn theme_fields_and_a_few_mapped_tokens() {
    let sample = palette(SAMPLE_CURRENT);
    let theme = oxa_theme(&sample, "tokyo-night", ColorMode::Ansi256);
    assert_eq!(theme.name, "tokyo-night");
    assert_eq!(theme.mode, ColorMode::Ansi256);
    assert_eq!(theme.appearance, Appearance::Dark);
    assert_eq!(theme.source, None);

    assert_eq!(rgb_of(&theme, "text"), sample.foreground);
    assert_eq!(rgb_of(&theme, "userMessageText"), sample.bright_foreground);
    assert_eq!(rgb_of(&theme, "accent"), sample.accent);
    assert_eq!(rgb_of(&theme, "selectedBg"), sample.selection);
    assert_eq!(rgb_of(&theme, "userMessageBg"), sample.lighter_background);
    assert_eq!(rgb_of(&theme, "error"), sample.red);
    assert_eq!(rgb_of(&theme, "toolDiffAdded"), sample.green);
    assert_eq!(rgb_of(&theme, "modePlan"), sample.yellow);
    assert_eq!(rgb_of(&theme, "syntaxNumber"), sample.orange);
    assert_eq!(rgb_of(&theme, "thinkingHigh"), sample.magenta);
    assert_eq!(
        rgb_of(&theme, "border"),
        mix_rgb(sample.background, sample.foreground, 0.40)
    );
    assert_eq!(
        rgb_of(&theme, "muted"),
        mix_rgb(sample.background, sample.foreground, 0.70)
    );
    assert_eq!(
        rgb_of(&theme, "dim"),
        mix_rgb(sample.background, sample.foreground, 0.55)
    );
    assert_eq!(
        rgb_of(&theme, "toolErrorBg"),
        mix_rgb(sample.background, sample.red, 0.12)
    );
}

#[test]
fn light_palette_gives_light_appearance() {
    let tmp = TempDir::new("light");
    let file = tmp.path().join("colors.toml");
    write(&file, LIGHT);
    let (p, _) = load_omarchy_palette(std::slice::from_ref(&file)).expect("palette");
    assert!(p.light);
    let theme = oxa_theme(&p, "light-one", ColorMode::TrueColor);
    assert_eq!(theme.appearance, Appearance::Light);
    assert_eq!(
        oxa_theme(&oxa_builtin_palette(), "oxa", ColorMode::TrueColor).appearance,
        Appearance::Dark
    );
}

#[test]
fn ansi256_theme_emits_256_colour_codes_and_truecolor_emits_rgb() {
    let sample = palette(SAMPLE_CURRENT);
    let ansi = oxa_theme(&sample, "t", ColorMode::Ansi256);
    let fg = ansi.fg("accent", "x");
    assert!(fg.contains("38;5;"), "no 256-colour code in {fg:?}");
    assert!(!fg.contains("38;2;"), "RGB code in 256-colour mode: {fg:?}");
    let bg = ansi.bg("selectedBg", "x");
    assert!(bg.contains("48;5;"), "no 256-colour background code in {bg:?}");

    let true_colour = oxa_theme(&sample, "t", ColorMode::TrueColor);
    let fg = true_colour.fg("accent", "x");
    assert!(fg.contains("38;2;122;162;247"), "no truecolour code in {fg:?}");
    assert!(
        !fg.contains("38;5;"),
        "256-colour code in truecolour mode: {fg:?}"
    );
}

#[test]
fn muted_and_dim_are_readable_on_the_background() {
    let sample = palette(SAMPLE_CURRENT);
    let builtin = oxa_builtin_palette();
    for p in [&sample, &builtin] {
        let theme = oxa_theme(p, "t", ColorMode::TrueColor);
        for token in ["muted", "dim"] {
            let ratio = contrast(rgb_of(&theme, token), p.background);
            assert!(
                ratio >= 3.0,
                "{token} contrast {ratio:.2} < 3.0 on {:?}",
                p.background
            );
        }
    }
}

#[test]
fn export_colours_are_lowercase_css_hex() {
    let sample = palette(SAMPLE_CURRENT);
    let theme = oxa_theme(&sample, "t", ColorMode::TrueColor);
    for key in ["pageBg", "cardBg", "infoBg"] {
        let value = theme
            .export
            .get(key)
            .unwrap_or_else(|| panic!("missing export {key}"));
        assert!(is_css_hex(value), "{key} = {value:?} is not #rrggbb");
    }
    assert_eq!(theme.export.get("pageBg").map(String::as_str), Some("#1a1b26"));
    assert_eq!(theme.export.get("cardBg").map(String::as_str), Some("#24283b"));

    let builtin_theme = oxa_theme(&oxa_builtin_palette(), "oxa", ColorMode::TrueColor);
    for value in builtin_theme.export.values() {
        assert!(is_css_hex(value), "builtin export {value:?} is not #rrggbb");
    }
    assert_eq!(builtin_theme.export.len(), 3);
}

#[test]
fn builtin_palette_values_are_fixed() {
    let b = oxa_builtin_palette();
    assert!(!b.light);
    assert_eq!(b.name, None);
    assert_eq!(b.background, (0x14, 0x16, 0x1b));
    assert_eq!(b.accent, (0xf2, 0xb8, 0x4b));
    assert_eq!(b.cyan, (0x5e, 0xc4, 0xc9));
}

// ---- O-185: following the desktop palette while running ---------------------------------------

/// A fake Omarchy state directory: `<root>/omarchy/current/theme/colors.toml` and the stamp.
fn fake_desktop(root: &Path, colors: &str, generation: &str) -> Vec<PathBuf> {
    let theme = root.join("state/omarchy/current/theme");
    std::fs::create_dir_all(&theme).expect("theme dir");
    std::fs::write(theme.join("colors.toml"), colors).expect("colors");
    std::fs::write(root.join("state/omarchy/current/theme.generation"), generation).expect("stamp");
    oxa_tui::omarchy_colors_paths(root, Some(&root.join("state")), Some(&root.join("config")))
}

const RED_THEME: &str = "background = \"#200000\"\nforeground = \"#ffe0e0\"\naccent = \"#ff2020\"\n";

#[test]
fn watch_reports_a_settled_change_once() {
    let tmp = TempDir::new("watch-change");
    let candidates = fake_desktop(&tmp.0, SAMPLE_CURRENT, "1");
    let mut watch = oxa_tui::PaletteWatch::new(candidates);
    assert!(watch.poll().is_none(), "nothing changed yet");
    assert_eq!(watch.interval_ms(), oxa_tui::PaletteWatch::IDLE_MS);

    fake_desktop(&tmp.0, RED_THEME, "2");
    assert!(
        watch.poll().is_none(),
        "the first sight of a change waits for it to settle"
    );
    assert_eq!(watch.interval_ms(), oxa_tui::PaletteWatch::SETTLE_MS);
    let (palette, path) = watch.poll().expect("the settled change is reported");
    assert_eq!(palette.accent, (0xff, 0x20, 0x20));
    assert!(path.ends_with("omarchy/current/theme/colors.toml"));
    assert!(watch.poll().is_none(), "a change is reported once");
    assert_eq!(watch.interval_ms(), oxa_tui::PaletteWatch::IDLE_MS);
}

#[test]
fn watch_keeps_quiet_on_a_half_written_file_and_recovers() {
    let tmp = TempDir::new("watch-half");
    let candidates = fake_desktop(&tmp.0, SAMPLE_CURRENT, "1");
    let mut watch = oxa_tui::PaletteWatch::new(candidates);
    // A half-written file: the foreground line is cut off.
    fake_desktop(&tmp.0, "background = \"#200000\"\nforegr", "2");
    assert!(watch.poll().is_none());
    assert!(
        watch.poll().is_none(),
        "a settled file that does not parse reports nothing"
    );
    assert!(
        watch.poll().is_none(),
        "and is not retried while it stays the same"
    );
    // The writer finishes.
    fake_desktop(&tmp.0, RED_THEME, "2");
    assert!(watch.poll().is_none());
    assert!(watch.poll().is_some(), "the completed file is picked up");
}

#[test]
fn watch_waits_while_files_keep_moving() {
    let tmp = TempDir::new("watch-moving");
    let candidates = fake_desktop(&tmp.0, SAMPLE_CURRENT, "1");
    let mut watch = oxa_tui::PaletteWatch::new(candidates);
    for generation in ["2", "3", "4"] {
        fake_desktop(&tmp.0, RED_THEME, generation);
        assert!(watch.poll().is_none(), "generation {generation} is still moving");
    }
    assert!(watch.poll().is_some(), "reported once it holds still");
}

#[test]
fn watch_sees_a_swapped_theme_link_and_a_palette_that_appears_later() {
    let tmp = TempDir::new("watch-link");
    // No palette at the start.
    let candidates =
        oxa_tui::omarchy_colors_paths(&tmp.0, Some(&tmp.0.join("state")), Some(&tmp.0.join("config")));
    let mut watch = oxa_tui::PaletteWatch::new(candidates.clone());
    assert!(watch.poll().is_none());
    // `current/theme` is a link to a theme directory (older Omarchy), with old timestamps.
    for (name, colors) in [("alpha", SAMPLE_CURRENT), ("beta", RED_THEME)] {
        let dir = tmp.0.join("themes").join(name);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join("colors.toml"), colors).expect("colors");
    }
    let current = tmp.0.join("state/omarchy/current");
    std::fs::create_dir_all(&current).expect("current");
    std::os::unix::fs::symlink(tmp.0.join("themes/alpha"), current.join("theme")).expect("link");
    assert!(watch.poll().is_none());
    assert!(
        watch.poll().is_some(),
        "a palette that appears later is picked up"
    );
    // The link is swapped to another theme: same file name, another target.
    std::fs::remove_file(current.join("theme")).expect("unlink");
    std::os::unix::fs::symlink(tmp.0.join("themes/beta"), current.join("theme")).expect("link");
    assert!(watch.poll().is_none());
    let (palette, _) = watch.poll().expect("the swapped link is a change");
    assert_eq!(palette.accent, (0xff, 0x20, 0x20));
    assert_eq!(palette.name.as_deref(), Some("beta"));
    // The palette disappears: nothing is reported, the caller keeps what it has.
    std::fs::remove_file(current.join("theme")).expect("unlink");
    assert!(watch.poll().is_none());
    assert!(watch.poll().is_none());
}
