//! The arcade animation of the activity line (O-184, ARCADE-ANIMATION.md): golden frames per
//! state, no motion with `ui.animation = "off"` or without colour, and the layout at narrow
//! widths. Public API only.
use oxa_config::Keymap;
use oxa_interactive::{
    arcade_scene, oxa_activity_line_with, oxa_arcade_line, scene_cells, tool_icon, Activity, Animation,
    ArcadeRun, ArcadeScene, Finish, Skin, StatusModel, SPRITE_FRAME_MS,
};
use oxa_tui::{strip_ansi, visible_width, ColorMode, Theme};

fn colour() -> Theme {
    oxa_tui::builtin_theme("dark", ColorMode::TrueColor, None).expect("builtin dark theme")
}

fn status(activity: Activity, frame: u64) -> StatusModel {
    StatusModel {
        activity,
        activity_ms: frame * SPRITE_FRAME_MS,
        ..StatusModel::default()
    }
}

fn tool(name: &str) -> Activity {
    Activity::Tool(name.to_string())
}

/// One of each activity a scene is drawn for.
fn activities() -> Vec<Activity> {
    vec![
        Activity::Thinking,
        Activity::Streaming,
        tool("bash"),
        Activity::Compacting,
        Activity::Prefill {
            done: 3,
            total: 8,
            secs: 0,
        },
        Activity::Retrying {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
        },
        Activity::Queued {
            pos: Some(2),
            wait_s: 0,
        },
    ]
}

const SCENES: [ArcadeScene; 3] = [ArcadeScene::Charge, ArcadeScene::Stars, ArcadeScene::Stack];
const RUN: ArcadeRun = ArcadeRun {
    coins: 3,
    finish: None,
};

/// The frames `0..n` of a scene, unstyled, each between bars.
fn frames(scene: ArcadeScene, activity: &Activity, cells: usize, skin: Skin, n: u64) -> Vec<String> {
    (0..n)
        .map(|f| {
            let st = status(activity.clone(), f);
            let row = arcade_scene(scene, &st, &RUN, cells, &colour(), skin).unwrap_or_default();
            format!("|{}|", strip_ansi(&row))
        })
        .collect()
}

fn check(what: &str, scene: ArcadeScene, activity: Activity, skin: Skin, cells: usize, golden: &[&str]) {
    let got = frames(scene, &activity, cells, skin, golden.len() as u64);
    assert_eq!(got, golden, "{what}:\n{}", got.join("\n"));
}

fn line(
    st: &StatusModel,
    animation: Animation,
    run: &ArcadeRun,
    width: u16,
    theme: &Theme,
    skin: Skin,
) -> Option<String> {
    oxa_arcade_line(
        st,
        &Keymap::defaults(),
        animation,
        ArcadeScene::Charge,
        run,
        width,
        theme,
        skin,
    )
}

// ---------------------------------------------------------------------------------------------
// Golden frames

#[test]
fn golden_frames_of_the_charging_ox() {
    check(
        "Charge Oxa 12 thinking",
        ArcadeScene::Charge,
        Activity::Thinking,
        Skin::Oxa,
        12,
        &[
            "|▟█▛▘  ▄  ▖  |",
            "|▟█▛▘  ▄  ▖  |",
            "|▜█▛▘ ▄  ▖   |",
            "|▜█▛▘ ▄  ▖   |",
            "|▟█▛▘▚  ▖   ▆|",
            "|▟█▛▘▞  ▖   ▆|",
            "|▜█▛▘  ▖   ▆ |",
            "|▜█▛▘  ▖   ▆ |",
            "|▟█▛▘ ▖   ▆  |",
            "|▟█▛▘ ▖   ▆  |",
            "|▜█▛▘▚   ▆  ▄|",
            "|▜█▛▘▞   ▆  ▄|",
        ],
    );
    check(
        "Charge Oxa 12 writing",
        ArcadeScene::Charge,
        Activity::Streaming,
        Skin::Oxa,
        12,
        &[
            "|▟█▛▘        |",
            "|▟█▛▘        |",
            "|░▜█▛▘       |",
            "|░▜█▛▘       |",
            "|▂░▟█▛▘      |",
            "|▂░▟█▛▘      |",
            "|▂▂░▜█▛▘     |",
            "|▂▂░▜█▛▘     |",
        ],
    );
    check(
        "Charge Oxa 12 tool bash",
        ArcadeScene::Charge,
        tool("bash"),
        Skin::Oxa,
        12,
        &[
            "|▟█▛▘ ▐$▌  ◆3|",
            "| ▟█▛▘▐$▌  ◆3|",
            "| ▜█▛▘ ▐$▌ ◆3|",
            "|▜█▛▘ ▐$▌  ◆3|",
        ],
    );
    check(
        "Charge Oxa 12 compacting",
        ArcadeScene::Charge,
        Activity::Compacting,
        Skin::Oxa,
        12,
        &[
            "|▐██████████▌|",
            "|▐█████████▌ |",
            "|▐████████▌  |",
            "|▐███████▌   |",
            "|▐██████▌    |",
            "|▐█████▌     |",
            "|▐████▌ SAVE |",
            "|▐███▌       |",
            "|▐██▌ SAVE   |",
            "|▐█▌         |",
            "|▐█▌ SAVE    |",
            "|▐█▌         |",
        ],
    );
    check(
        "Charge Oxa 12 prefill",
        ArcadeScene::Charge,
        Activity::Prefill {
            done: 3,
            total: 8,
            secs: 0,
        },
        Skin::Oxa,
        12,
        &["|  ▟█▛▘█·····|", "|  ▟█▛▘█·····|"],
    );
    check(
        "Charge Oxa 12 retry",
        ArcadeScene::Charge,
        Activity::Retrying {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
        },
        Skin::Oxa,
        12,
        &[
            "|▟█▛▘ ♥♥·    |",
            "|▟█▛▘ ♥♥·    |",
            "|▟█▛▘ ♥♥·    |",
            "|     ♥♥·    |",
        ],
    );
    check(
        "Charge Oxa 12 queued",
        ArcadeScene::Charge,
        Activity::Queued {
            pos: Some(2),
            wait_s: 0,
        },
        Skin::Oxa,
        12,
        &[
            "|▟█▛▘ z      |",
            "|▟█▛▘ z      |",
            "|▟█▛▘ z      |",
            "|▟█▛▘ z      |",
            "|▟█▛▘  z     |",
            "|▟█▛▘  z     |",
            "|▟█▛▘  z     |",
            "|▟█▛▘  z     |",
            "|▟█▛▘   Z    |",
        ],
    );
}

#[test]
fn golden_frames_at_eight_cells() {
    check(
        "Charge Oxa 8 thinking",
        ArcadeScene::Charge,
        Activity::Thinking,
        Skin::Oxa,
        8,
        &[
            "|▟█▛▘  ▄ |",
            "|▟█▛▘  ▄ |",
            "|▜█▛▘ ▄  |",
            "|▜█▛▘ ▄  |",
            "|▟█▛▘▚  ▖|",
            "|▟█▛▘▞  ▖|",
            "|▜█▛▘  ▖ |",
            "|▜█▛▘  ▖ |",
            "|▟█▛▘ ▖  |",
            "|▟█▛▘ ▖  |",
            "|▜█▛▘▚   |",
            "|▜█▛▘▞   |",
        ],
    );
    check(
        "Charge Oxa 8 tool bash",
        ArcadeScene::Charge,
        tool("bash"),
        Skin::Oxa,
        8,
        &["|▟█▛▘ ▐$▌|", "| ▟█▛▘▐$▌|", "| ▜█▛▘▐$▌|", "|▜█▛▘ ▐$▌|"],
    );
    check(
        "Charge Oxa 8 compacting",
        ArcadeScene::Charge,
        Activity::Compacting,
        Skin::Oxa,
        8,
        &[
            "|▐██████▌|",
            "|▐█████▌ |",
            "|▐████▌  |",
            "|▐███▌   |",
            "|▐██▌    |",
            "|▐█▌     |",
            "|▐█▌ SAVE|",
            "|▐█▌     |",
            "|▐█▌ SAVE|",
            "|▐█▌     |",
            "|▐█▌ SAVE|",
            "|▐█▌     |",
        ],
    );
}

#[test]
fn golden_frames_in_ascii() {
    check(
        "Charge OxaAscii 12 thinking",
        ArcadeScene::Charge,
        Activity::Thinking,
        Skin::OxaAscii,
        12,
        &[
            "|{ox>  n  o  |",
            "|{ox>  n  o  |",
            "|(ox> n  o   |",
            "|(ox> n  o   |",
            "|{ox>*  o   H|",
            "|{ox>%  o   H|",
            "|(ox>  o   H |",
            "|(ox>  o   H |",
            "|{ox> o   H  |",
            "|{ox> o   H  |",
            "|(ox>*   H  n|",
            "|(ox>%   H  n|",
        ],
    );
    check(
        "Charge OxaAscii 12 writing",
        ArcadeScene::Charge,
        Activity::Streaming,
        Skin::OxaAscii,
        12,
        &[
            "|{ox>        |",
            "|{ox>        |",
            "|~(ox>       |",
            "|~(ox>       |",
            "|_~{ox>      |",
            "|_~{ox>      |",
            "|__~(ox>     |",
            "|__~(ox>     |",
        ],
    );
    check(
        "Charge OxaAscii 12 tool bash",
        ArcadeScene::Charge,
        tool("bash"),
        Skin::OxaAscii,
        12,
        &[
            "|{ox> [$]  o3|",
            "| {ox>[$]  o3|",
            "| (ox> [$] o3|",
            "|(ox> [$]  o3|",
        ],
    );
    check(
        "Charge OxaAscii 12 compacting",
        ArcadeScene::Charge,
        Activity::Compacting,
        Skin::OxaAscii,
        12,
        &[
            "|[##########]|",
            "|[#########] |",
            "|[########]  |",
            "|[#######]   |",
            "|[######]    |",
            "|[#####]     |",
            "|[####] SAVE |",
            "|[###]       |",
            "|[##] SAVE   |",
            "|[#]         |",
            "|[#] SAVE    |",
            "|[#]         |",
        ],
    );
    check(
        "Charge OxaAscii 12 prefill",
        ArcadeScene::Charge,
        Activity::Prefill {
            done: 3,
            total: 8,
            secs: 0,
        },
        Skin::OxaAscii,
        12,
        &["|  {ox>#.....|", "|  {ox>#.....|"],
    );
    check(
        "Charge OxaAscii 12 retry",
        ArcadeScene::Charge,
        Activity::Retrying {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
        },
        Skin::OxaAscii,
        12,
        &[
            "|{ox> vv.    |",
            "|{ox> vv.    |",
            "|{ox> vv.    |",
            "|     vv.    |",
        ],
    );
    check(
        "Charge OxaAscii 12 queued",
        ArcadeScene::Charge,
        Activity::Queued {
            pos: Some(2),
            wait_s: 0,
        },
        Skin::OxaAscii,
        12,
        &[
            "|{ox> z      |",
            "|{ox> z      |",
            "|{ox> z      |",
            "|{ox> z      |",
            "|{ox>  z     |",
            "|{ox>  z     |",
            "|{ox>  z     |",
            "|{ox>  z     |",
            "|{ox>   Z    |",
        ],
    );
}

#[test]
fn golden_frames_of_the_other_scenes() {
    check(
        "Stars Oxa 12 thinking",
        ArcadeScene::Stars,
        Activity::Thinking,
        Skin::Oxa,
        12,
        &[
            "| ═▶    *    |",
            "| ═▶    *    |",
            "| ═▶   *·    |",
            "| ═▶   *·    |",
            "| ·═▶ *·    ·|",
            "| ·═▶ *·    ·|",
            "| ·═▶* ·    *|",
            "| ·═▶* ·    *|",
            "| ═▶* ·    * |",
            "| ═▶* ·    * |",
            "| ═▶  ·   *· |",
            "| ═▶  ·   *· |",
        ],
    );
    check(
        "Stars Oxa 12 writing",
        ArcadeScene::Stars,
        Activity::Streaming,
        Skin::Oxa,
        12,
        &[
            "| ═▶    ━    |",
            "| ═▶  ━─    ─|",
            "| ═▶━ ─    ━ |",
            "| ═▶ ─   ━─  |",
            "|  ═▶  ━ ─   |",
            "|  ═▶━  ─   ━|",
            "| ─═▶  ─  ━ ─|",
            "|  ═▶ ─ ━  ─ |",
        ],
    );
    check(
        "Stars Oxa 12 tool bash",
        ArcadeScene::Stars,
        tool("bash"),
        Skin::Oxa,
        12,
        &[
            "|* ·═▶▐$▌  ◆3|",
            "|* ·═▶▐$▌  ◆3|",
            "|  ═▶ ▐$▌  ◆3|",
            "|  ═▶ ▐$▌  ◆3|",
        ],
    );
    check(
        "Stack Oxa 12 thinking",
        ArcadeScene::Stack,
        Activity::Thinking,
        Skin::Oxa,
        12,
        &[
            "|            |",
            "|▂           |",
            "|▄           |",
            "|▆           |",
            "|█           |",
            "|█▂          |",
            "|█▄          |",
            "|█▆          |",
            "|██          |",
            "|██▂         |",
            "|██▄         |",
            "|██▆         |",
        ],
    );
    check(
        "Stack Oxa 12 writing",
        ArcadeScene::Stack,
        Activity::Streaming,
        Skin::Oxa,
        12,
        &[
            "|            |",
            "|▄           |",
            "|█           |",
            "|█▄          |",
            "|██          |",
            "|██▄         |",
            "|███         |",
            "|███▄        |",
        ],
    );
    check(
        "Stack Oxa 12 tool bash",
        ArcadeScene::Stack,
        tool("bash"),
        Skin::Oxa,
        12,
        &[
            "|      ▐$▌ ◆3|",
            "|▂     ▐$▌ ◆3|",
            "|▄     ▐$▌ ◆3|",
            "|▆     ▐$▌ ◆3|",
        ],
    );
    check(
        "Stars OxaAscii 12 thinking",
        ArcadeScene::Stars,
        Activity::Thinking,
        Skin::OxaAscii,
        12,
        &[
            "| =>    *    |",
            "| =>    *    |",
            "| =>   *.    |",
            "| =>   *.    |",
            "| .=> *.    .|",
            "| .=> *.    .|",
            "| .=>* .    *|",
            "| .=>* .    *|",
            "| =>* .    * |",
            "| =>* .    * |",
            "| =>  .   *. |",
            "| =>  .   *. |",
        ],
    );
    check(
        "Stack OxaAscii 12 thinking",
        ArcadeScene::Stack,
        Activity::Thinking,
        Skin::OxaAscii,
        12,
        &[
            "|            |",
            "|.           |",
            "|:           |",
            "|=           |",
            "|#           |",
            "|#.          |",
            "|#:          |",
            "|#=          |",
            "|##          |",
            "|##.         |",
            "|##:         |",
            "|##=         |",
        ],
    );
}

#[test]
fn golden_closing_scenes() {
    let st = status(Activity::Idle, 0);
    let shown = |how: Finish, coins: u32, frame: u64, skin: Skin| {
        let run = ArcadeRun {
            coins,
            finish: Some((how, frame * SPRITE_FRAME_MS)),
        };
        strip_ansi(&line(&st, Animation::Arcade, &run, 80, &colour(), skin).unwrap_or_default())
    };
    assert_eq!(
        shown(Finish::Clear, 3, 0, Skin::Oxa),
        " ▟█▛▘  + * ·   CLEAR   ◆3 tools"
    );
    assert_eq!(
        shown(Finish::Clear, 3, 2, Skin::Oxa),
        " ▟█▛▘ · + * ·  CLEAR   ◆3 tools"
    );
    assert_eq!(
        shown(Finish::Clear, 1, 0, Skin::Oxa),
        " ▟█▛▘  + * ·   CLEAR   ◆1 tool"
    );
    assert_eq!(shown(Finish::Clear, 0, 0, Skin::Oxa), " ▟█▛▘  + * ·   CLEAR ");
    assert_eq!(shown(Finish::Fail, 0, 0, Skin::Oxa), " ▟█▛▘ ✗        FAILED ");
    assert_eq!(shown(Finish::Fail, 0, 3, Skin::Oxa), " ▟█▛▘          FAILED ");
    assert!(shown(Finish::Clear, 3, 0, Skin::OxaAscii).is_ascii());
    // Idle with no closing scene (it is over, or the run was aborted): no line at all.
    assert!(line(
        &st,
        Animation::Arcade,
        &ArcadeRun::default(),
        80,
        &colour(),
        Skin::Oxa
    )
    .is_none());
    // The closing scene is the arcade's: nothing shows without it.
    let done = ArcadeRun {
        coins: 3,
        finish: Some((Finish::Clear, 0)),
    };
    assert!(line(&st, Animation::Off, &done, 80, &colour(), Skin::Oxa).is_none());
    assert!(line(&st, Animation::Minimal, &done, 80, &colour(), Skin::Oxa).is_none());
    assert!(line(&st, Animation::Arcade, &done, 80, &Theme::plain(), Skin::Oxa).is_none());
}

// ---------------------------------------------------------------------------------------------
// Properties of every frame

#[test]
fn every_frame_is_exactly_as_wide_as_the_scene() {
    for scene in SCENES {
        for skin in [Skin::Oxa, Skin::OxaAscii] {
            for cells in [8usize, 12] {
                for activity in activities() {
                    for f in 0..200u64 {
                        let st = status(activity.clone(), f);
                        let row = arcade_scene(scene, &st, &RUN, cells, &colour(), skin)
                            .unwrap_or_else(|| panic!("{scene:?} {activity:?}: a scene"));
                        assert_eq!(
                            visible_width(&row),
                            cells,
                            "{scene:?} {skin:?} {activity:?} frame {f}: {:?}",
                            strip_ansi(&row)
                        );
                        assert!(!row.contains('\n'));
                        if skin == Skin::OxaAscii {
                            assert!(strip_ansi(&row).is_ascii(), "{scene:?} {activity:?}: {row:?}");
                        }
                    }
                    // A frame lasts SPRITE_FRAME_MS: the same picture within it, and it moves.
                    let at = |ms: u64| {
                        let st = StatusModel {
                            activity: activity.clone(),
                            activity_ms: ms,
                            ..StatusModel::default()
                        };
                        arcade_scene(scene, &st, &RUN, cells, &colour(), skin)
                    };
                    assert_eq!(at(0), at(SPRITE_FRAME_MS - 1));
                    assert!(at(u64::MAX).is_some(), "no overflow at the end of time");
                }
            }
        }
    }
    assert_eq!(SPRITE_FRAME_MS, 125, "8 frames per second, not more");
}

#[test]
fn the_scenes_move_and_the_states_differ() {
    for scene in SCENES {
        let mut first: Vec<Vec<String>> = Vec::new();
        for activity in [Activity::Thinking, Activity::Streaming, tool("bash")] {
            let seen = frames(scene, &activity, 12, Skin::Oxa, 48);
            let distinct: std::collections::BTreeSet<&String> = seen.iter().collect();
            assert!(distinct.len() >= 3, "{scene:?} {activity:?} moves: {distinct:?}");
            first.push(seen);
        }
        assert!(first[0] != first[1] && first[1] != first[2] && first[0] != first[2]);
    }
    // Nothing to animate: idle, waiting for the user's answer, no room.
    let run = ArcadeRun::default();
    for activity in [Activity::Idle, Activity::Blocked] {
        let st = status(activity, 3);
        assert!(arcade_scene(ArcadeScene::Charge, &st, &run, 12, &colour(), Skin::Oxa).is_none());
    }
    let st = status(Activity::Thinking, 3);
    assert!(arcade_scene(ArcadeScene::Charge, &st, &run, 0, &colour(), Skin::Oxa).is_none());
}

#[test]
fn a_tool_has_its_crate_and_the_run_its_coins() {
    for (name, icon) in [
        ("bash", '$'),
        ("read", '≡'),
        ("edit", '±'),
        ("write", '+'),
        ("grep", '?'),
    ] {
        assert_eq!(tool_icon(name, false), icon);
        let st = status(tool(name), 0);
        let row = arcade_scene(ArcadeScene::Charge, &st, &RUN, 12, &colour(), Skin::Oxa).unwrap_or_default();
        assert!(strip_ansi(&row).contains(&format!("▐{icon}▌")), "{name}: {row:?}");
    }
    let coins = |n: u32| {
        let run = ArcadeRun {
            coins: n,
            finish: None,
        };
        let row = arcade_scene(
            ArcadeScene::Charge,
            &status(tool("bash"), 0),
            &run,
            12,
            &colour(),
            Skin::Oxa,
        );
        strip_ansi(&row.unwrap_or_default())
    };
    assert_eq!(coins(0), "▟█▛▘ ▐$▌    ");
    assert_eq!(coins(7), "▟█▛▘ ▐$▌  ◆7");
    assert_eq!(coins(42), "▟█▛▘ ▐$▌ ◆42");
    assert_eq!(coins(5000), "▟█▛▘ ▐$▌ ◆99");
}

#[test]
fn colour_comes_from_the_theme_tokens_only() {
    let theme = colour();
    let st = status(Activity::Thinking, 4);
    let row = arcade_scene(ArcadeScene::Charge, &st, &RUN, 12, &theme, Skin::Oxa).unwrap_or_default();
    assert!(
        row.contains(&theme.fg_code("thinkingHigh")),
        "the ox has the state's colour"
    );
    assert!(row.contains(&theme.fg_code("muted")) && row.contains(&theme.fg_code("warning")));
    // Another palette, other codes: nothing is hard-coded.
    let light = oxa_tui::builtin_theme("light", ColorMode::TrueColor, None).expect("builtin light theme");
    let other = arcade_scene(ArcadeScene::Charge, &st, &RUN, 12, &light, Skin::Oxa).unwrap_or_default();
    assert_ne!(row, other);
    assert_eq!(strip_ansi(&row), strip_ansi(&other));
    // O-182: the hero burns while YOLO is on.
    let hot = StatusModel {
        yolo: true,
        ..status(Activity::Thinking, 4)
    };
    let burning = arcade_scene(ArcadeScene::Charge, &hot, &RUN, 12, &theme, Skin::Oxa).unwrap_or_default();
    assert!(burning.contains(&theme.fg_code("error")) && !burning.contains(&theme.fg_code("thinkingHigh")));
}

// ---------------------------------------------------------------------------------------------
// Off, NO_COLOR, minimal

/// `true` when the text holds a glyph of a scene or of a sprite.
fn has_scene_glyph(text: &str) -> bool {
    text.chars().any(|c| "▟▜▛▘▄▆▖▚▞▂▐▌◆♥═▶▪▒▙▁▃▅▇".contains(c)) || text.contains("ox>")
}

#[test]
fn off_and_no_color_draw_no_animation() {
    let km = Keymap::defaults();
    for activity in activities() {
        for skin in [Skin::Oxa, Skin::OxaAscii] {
            // `ui.animation = "off"` (also reduced motion): the static badge, the same bytes on
            // every frame of a second, and exactly the line without motion.
            let theme = colour();
            let off: Vec<String> = (0..8)
                .map(|f| {
                    line(
                        &status(activity.clone(), f),
                        Animation::Off,
                        &RUN,
                        100,
                        &theme,
                        skin,
                    )
                    .unwrap_or_default()
                })
                .collect();
            assert!(
                off.iter().all(|l| l == &off[0]),
                "{activity:?}: off does not move"
            );
            assert!(
                !has_scene_glyph(&strip_ansi(&off[0])),
                "{activity:?}: {:?}",
                off[0]
            );
            let st = status(activity.clone(), 0);
            assert_eq!(
                Some(off[0].clone()),
                oxa_activity_line_with(&st, &km, false, 100, &theme, skin)
            );

            // NO_COLOR / TERM=dumb (the plain theme): no escape sequence at all, whatever the
            // setting says, and nothing moves.
            let plain = Theme::plain();
            for animation in [Animation::Arcade, Animation::Minimal, Animation::Off] {
                let lines: Vec<String> = (0..8)
                    .map(|f| {
                        line(&status(activity.clone(), f), animation, &RUN, 100, &plain, skin)
                            .unwrap_or_default()
                    })
                    .collect();
                for l in &lines {
                    assert!(
                        !l.contains('\x1b'),
                        "{activity:?} {animation:?}: an escape in {l:?}"
                    );
                    assert!(!has_scene_glyph(l), "{activity:?} {animation:?}: {l:?}");
                    assert_eq!(l, &lines[0], "{activity:?} {animation:?}: it moved");
                }
                assert!(lines[0].starts_with(" ["), "the text badge: {:?}", lines[0]);
            }
        }
    }
}

#[test]
fn minimal_is_the_small_sprite_and_waiting_never_animates() {
    let (km, theme) = (Keymap::defaults(), colour());
    let st = status(Activity::Thinking, 3);
    assert_eq!(
        line(&st, Animation::Minimal, &RUN, 100, &theme, Skin::Oxa),
        oxa_activity_line_with(&st, &km, true, 100, &theme, Skin::Oxa)
    );
    // Waiting for the user's answer: the badge, in every mode.
    let waiting = status(Activity::Blocked, 3);
    for animation in [Animation::Arcade, Animation::Minimal, Animation::Off] {
        let l = line(&waiting, animation, &RUN, 100, &theme, Skin::Oxa).unwrap_or_default();
        assert!(!has_scene_glyph(&strip_ansi(&l)), "{animation:?}: {l:?}");
        assert!(strip_ansi(&l).starts_with("  WAITING "), "{l:?}");
    }
    // pi's look has no arcade.
    assert!(line(&st, Animation::Arcade, &RUN, 100, &theme, Skin::Pi)
        .is_some_and(|l| !has_scene_glyph(&strip_ansi(&l))));
}

// ---------------------------------------------------------------------------------------------
// Layout

#[test]
fn one_line_that_fits_from_sixty_columns_up() {
    let long = StatusModel {
        working_message: Some(
            "reading 4 files 2/4 and a working text that is far too long to fit".to_string(),
        ),
        ..status(tool("read"), 81)
    };
    for scene in SCENES {
        for skin in [Skin::Oxa, Skin::OxaAscii] {
            for width in 1..=140u16 {
                for st in activities()
                    .into_iter()
                    .map(|a| status(a, 81))
                    .chain([long.clone()])
                {
                    let l = oxa_arcade_line(
                        &st,
                        &Keymap::defaults(),
                        Animation::Arcade,
                        scene,
                        &RUN,
                        width,
                        &colour(),
                        skin,
                    )
                    .unwrap_or_default();
                    assert!(
                        visible_width(&l) <= usize::from(width),
                        "{scene:?} {width}: {:?}",
                        strip_ansi(&l)
                    );
                    assert!(!l.contains('\n'), "one line");
                }
            }
        }
    }
    // At 60 columns everything is still there: the scene (8 cells), the badge, the time and
    // the cancel key.
    assert_eq!(
        (scene_cells(59), scene_cells(60), scene_cells(71), scene_cells(72)),
        (8, 8, 8, 12)
    );
    assert_eq!((scene_cells(39), scene_cells(40)), (0, 8));
    let st = StatusModel {
        activity: Activity::Thinking,
        activity_ms: 8000,
        ..StatusModel::default()
    };
    let at60 = strip_ansi(&line(&st, Animation::Arcade, &RUN, 60, &colour(), Skin::Oxa).unwrap_or_default());
    assert_eq!(at60, " ▟█▛▘  ▄   THINKING   8s · escape to cancel");
    let at80 = strip_ansi(&line(&st, Animation::Arcade, &RUN, 80, &colour(), Skin::Oxa).unwrap_or_default());
    assert_eq!(at80, " ▟█▛▘  ▄  ▖    THINKING   8s · escape to cancel");
    // The line is as high with the scene as without it: one row, so nothing below or above
    // it moves when the animation is switched.
    let narrow = line(&st, Animation::Arcade, &RUN, 30, &colour(), Skin::Oxa).unwrap_or_default();
    assert_eq!(strip_ansi(&narrow), "  THINKING   8s · escape to...");
}

// ---------------------------------------------------------------------------------------------
// Cost (run with `--ignored --nocapture`, in release, to print the numbers)

#[test]
#[ignore = "a measurement, not a gate: cargo test --release -- --ignored --nocapture frame_cost"]
fn frame_cost() {
    let (km, theme) = (Keymap::defaults(), colour());
    for (name, animation) in [
        ("arcade", Animation::Arcade),
        ("minimal", Animation::Minimal),
        ("off", Animation::Off),
    ] {
        for activity in [
            Activity::Thinking,
            Activity::Streaming,
            tool("bash"),
            Activity::Compacting,
        ] {
            let rounds = 200_000u64;
            let mut bytes = 0usize;
            let start = std::time::Instant::now();
            for f in 0..rounds {
                let st = status(activity.clone(), f);
                let l = oxa_arcade_line(
                    &st,
                    &km,
                    animation,
                    ArcadeScene::Charge,
                    &RUN,
                    100,
                    &theme,
                    Skin::Oxa,
                );
                bytes += l.map_or(0, |l| l.len());
            }
            let ns = start.elapsed().as_nanos() as f64 / rounds as f64;
            println!(
                "frame_cost {name:8} {activity:?}: {ns:.0} ns per frame, {} bytes per frame, {:.4} % of one core at 8 fps",
                bytes / rounds as usize,
                ns * 8.0 / 1e9 * 100.0
            );
        }
    }
}
