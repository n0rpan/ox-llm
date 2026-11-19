//! Scene data and validation (design 4.1 to 4.3): the built-in pack loads whole, every frame is
//! as wide as its size key, a bad scene is skipped with its reason, the ASCII map keeps the
//! width, and only the allow-listed glyphs get through.

use oxa_interactive::skin::stage::{ascii_text, built_in, glyph_allowed, load_pack, Tier};
use oxa_tui::visible_width;
use serde_json::{json, Value};

fn pack(scenes: Vec<Value>) -> String {
    json!({ "version": 1, "scenes": scenes }).to_string()
}

/// A valid run scene: 8 and 12 cells, one frame each.
fn run_scene(id: &str) -> Value {
    json!({
        "id": id,
        "title": "a run",
        "on": ["think"],
        "tier": "run",
        "loop": "loop",
        "step_ms": 125,
        "significance": 0,
        "sizes": {
            "8": [["▟█▛▘ (o<", "HHHH.GGG"]],
            "12": [["▟█▛▘     (o<", "HHHH.....GGG"]]
        },
        "captions": [["ask the duck", "explaining it to the rubber duck"]]
    })
}

#[test]
fn built_in_pack_passes_validation() {
    let loaded = built_in();
    assert!(loaded.skipped.is_empty(), "skipped: {:?}", loaded.skipped);
    let ids: Vec<&str> = loaded.scenes.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "think.duck",
            "write.typewriter",
            "tool.bash.pipe",
            "win.clear.flag"
        ]
    );
    let win = loaded
        .scenes
        .iter()
        .find(|s| s.id == "win.clear.flag")
        .expect("the win scene");
    assert_eq!(win.tier, Tier::Win);
    assert_eq!(win.significance, 1);
}

#[test]
fn every_frame_is_as_wide_as_its_size() {
    for scene in built_in().scenes {
        let frame_sets = scene.sizes.iter().chain(scene.ascii.iter().flatten());
        for (size, frames) in frame_sets {
            assert!(!frames.is_empty(), "{} has no frames at {size}", scene.id);
            for frame in frames {
                assert_eq!(
                    visible_width(&frame.text),
                    *size,
                    "{} text {:?}",
                    scene.id,
                    frame.text
                );
                assert_eq!(
                    frame.inks.chars().count(),
                    *size,
                    "{} inks {:?}",
                    scene.id,
                    frame.inks
                );
            }
        }
    }
}

#[test]
fn a_bad_scene_is_skipped_with_a_reason() {
    let mut bad = run_scene("tool.bad.width");
    bad["sizes"]["8"] = json!([["▟█▛▘ (o", "HHHH.GG"]]);
    let text = pack(vec![run_scene("think.good"), bad]);
    let loaded = load_pack(&text).expect("the pack itself is valid");
    let ids: Vec<&str> = loaded.scenes.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["think.good"]);
    assert_eq!(loaded.skipped.len(), 1);
    assert_eq!(loaded.skipped[0].id, "tool.bad.width");
    assert!(
        loaded.skipped[0].reason.contains("cells wide"),
        "reason: {}",
        loaded.skipped[0].reason
    );
}

#[test]
fn a_pack_of_another_version_is_refused_whole() {
    let text = json!({ "version": 2, "scenes": [run_scene("think.good")] }).to_string();
    assert!(load_pack(&text).is_err());
    assert!(load_pack("not json").is_err());
}

#[test]
fn a_win_is_at_most_six_frames_at_significance_one() {
    let mut win = run_scene("win.too.long");
    win["tier"] = json!("win");
    win["significance"] = json!(1);
    win["loop"] = json!("once");
    let eight = vec![json!(["▟█▛▘  ▐▀", "HHHH..DD"]); 7];
    let twelve = vec![json!(["▟█▛▘      ▐▀", "HHHH......DD"]); 7];
    win["sizes"] = json!({ "8": eight, "12": twelve });
    let loaded = load_pack(&pack(vec![win])).expect("valid pack");
    assert!(loaded.scenes.is_empty());
    assert!(
        loaded.skipped[0].reason.contains("at most 6"),
        "reason: {}",
        loaded.skipped[0].reason
    );
}

#[test]
fn ascii_map_gives_ascii_of_the_same_width() {
    let text = "▟█▛▘ ▜█▛▘ ▝▜▙ ▐▀▪─────↵ ◐ ✓ ▂▂█";
    let ascii = ascii_text(text).expect("every glyph here has an ASCII form");
    assert!(ascii.is_ascii(), "{ascii:?}");
    assert_eq!(visible_width(&ascii), visible_width(text));
    assert_eq!(ascii_text("▟█▛▘").as_deref(), Some("{ox>"));
    assert_eq!(ascii_text("▪────▐$▌").as_deref(), Some("o----[$]"));
    // A glyph with no ASCII form leaves the scene out of the ASCII pools (4.3), it is not an error.
    assert_eq!(ascii_text("▘"), None);
    for scene in built_in().scenes {
        assert!(scene.ascii.is_some(), "{} has no ASCII form", scene.id);
    }
}

#[test]
fn glyphs_outside_the_allow_list_are_refused() {
    for c in ['A', '█', '▀', '─', '▶', '·', '↵', '≡', '✗'] {
        assert!(glyph_allowed(c), "{c:?} should be allowed");
    }
    for c in ['×', 'é', '✨', '🙂', '\u{1b}', '\t'] {
        assert!(!glyph_allowed(c), "{c:?} should be refused");
    }
    let mut odd = run_scene("think.odd");
    odd["sizes"]["8"] = json!([["▟█▛▘ (o×", "HHHH.GGG"]]);
    let loaded = load_pack(&pack(vec![odd])).expect("valid pack");
    assert!(loaded.scenes.is_empty());
    assert!(
        loaded.skipped[0].reason.contains("allow-list"),
        "reason: {}",
        loaded.skipped[0].reason
    );
}
