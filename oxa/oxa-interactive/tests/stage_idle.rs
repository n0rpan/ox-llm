//! The director in the loop (animations design 3.2 and 3.3, slice 4): zero work when idle, one
//! still frame while waiting for the user, slow and settled scenes step once a second, and a
//! pinned scene bypasses the picks. Public API only; the clock is passed in.
use oxa_interactive::skin::stage::director::{Director, Pending};
use oxa_interactive::skin::stage::load_pack;
use serde_json::{json, Value};

fn frame(text: &str, inks: &str) -> Value {
    json!([text, inks])
}

/// A run scene on `on`, two frames at 8 and 12 cells, looping every `step` ms.
fn scene(id: &str, on: &str, looping: &str, step: u32) -> Value {
    json!({
        "id": id,
        "title": "a run",
        "on": [on],
        "tier": "run",
        "loop": looping,
        "step_ms": step,
        "significance": 0,
        "sizes": {
            "8": [frame("(ox>    ", "HHHH...."), frame("ox>     ", "HHH.....")],
            "12": [frame("(ox>        ", "HHHH........"), frame("ox>         ", "HHH.........")]
        },
        "captions": [["a caption", "a longer caption"]]
    })
}

fn director(scenes: Vec<Value>) -> Director {
    let text = json!({ "version": 1, "scenes": scenes }).to_string();
    let loaded = load_pack(&text).expect("the test pack is a version 1 pack");
    assert!(
        loaded.skipped.is_empty(),
        "the test scenes must all load: {:?}",
        loaded.skipped
    );
    Director::new("session-1", loaded.scenes)
}

fn working(key: &str) -> Pending {
    Pending::Working {
        key: key.to_string(),
        icon: '*',
    }
}

#[test]
fn no_idle_cpu() {
    let mut d = director(vec![
        scene("think.duck", "think", "loop", 125),
        scene("done.flag", "done", "once", 125),
    ]);
    d.update(Pending::Idle, 8, 0);
    assert_eq!(d.next_deadline_ms(0), None);
    let mut redraws = 0;
    for t in 0..10_000u64 {
        if d.redraw_due(t) {
            redraws += 1;
        }
    }
    assert_eq!(redraws, 0, "an idle stage never asks to be drawn");

    // A working scene does have a deadline, so the loop can sleep until it.
    d.update(working("think"), 8, 10_000);
    assert_eq!(d.next_deadline_ms(10_000), Some(10_125));

    // A finished celebration (a `once` scene after its last frame) has no deadline either.
    d.update(working("done"), 8, 20_000);
    assert_eq!(d.shown(20_300), None);
    assert_eq!(d.next_deadline_ms(20_300), None);
}

#[test]
fn waiting_for_the_user_is_one_still_frame() {
    let mut d = director(vec![scene("think.duck", "think", "loop", 125)]);
    d.update(working("think"), 8, 0);
    assert!(d.redraw_due(0), "the first frame is drawn");
    d.update(Pending::Waiting, 8, 1_000);
    assert_eq!(d.next_deadline_ms(1_000), None, "waiting has no timer");
    assert!(d.redraw_due(1_000), "the scene is taken away once");
    let mut redraws = 0;
    for t in (1_000..20_000u64).step_by(50) {
        if d.redraw_due(t) {
            redraws += 1;
        }
    }
    assert_eq!(redraws, 0, "the poster does not move while the user is asked");
    assert_eq!(d.shown(15_000), None);
}

#[test]
fn a_slow_scene_steps_once_a_second() {
    let mut d = director(vec![scene("wait.hourglass", "wait", "loop", 125)]);
    d.update(working("wait:ci"), 8, 0);
    assert_eq!(d.shown(0), Some(("wait.hourglass", 0)));
    // The scene's own step is 125 ms; a wait key holds each frame for a second.
    assert_eq!(d.shown(124), Some(("wait.hourglass", 0)));
    assert_eq!(d.shown(999), Some(("wait.hourglass", 0)));
    assert_eq!(d.shown(1_000), Some(("wait.hourglass", 1)));
    assert_eq!(d.next_deadline_ms(0), Some(1_000));
}

#[test]
fn a_scene_settles_after_twenty_seconds() {
    let mut d = director(vec![scene("think.duck", "think", "loop", 125)]);
    d.update(working("think"), 8, 0);
    // Before the settle time the scene keeps its own 125 ms step.
    assert_eq!(d.next_deadline_ms(19_000), Some(19_125));
    // After twenty seconds of the same activity it steps once a second.
    assert_eq!(d.next_deadline_ms(20_000), Some(21_000));
    // A new run restarts the settle clock: idle, then the same key again.
    d.update(Pending::Idle, 8, 20_000);
    d.update(working("think"), 8, 20_000);
    assert_eq!(d.next_deadline_ms(20_000), Some(20_125));
}

#[test]
fn a_pinned_scene_bypasses_the_director() {
    let mut d = director(vec![
        scene("think.duck", "think", "loop", 125),
        scene("tool.bash", "tool:bash", "loop", 125),
    ]);
    d.pin(Some("tool.bash"));
    d.update(working("think"), 8, 0);
    assert_eq!(
        d.shown(0),
        Some(("tool.bash", 0)),
        "the pin plays whatever the activity is"
    );
    d.pin(None);
    d.update(Pending::Idle, 8, 1_000);
    d.update(working("think"), 8, 1_000);
    assert_eq!(
        d.shown(1_000),
        Some(("think.duck", 0)),
        "releasing the pin returns to the picks"
    );
}
