//! V022 (#884): the bundled syntax set can be warmed after the first frame instead of by the
//! first highlighted code block.
//!
//! This is the first slice of the change: the `oxa-tui` helper only. The interactive loop's call
//! site (one bounded load after the first editable frame, on a short-lived `oxa-syntax-warm`
//! thread) and its ordering assertion are not part of this file yet.
//!
//! Deliberately one test in its own binary: `oxa_tui`'s syntax set is a process-wide `OnceLock`,
//! so "not loaded before the warm-up" is only a meaningful statement when nothing else in the
//! same process can load it first. Add no second test here.

/// The helper's contract: the set starts unloaded (it is loaded lazily, never at startup), the
/// first warm call does the load and says so, every later call is a no-op that says so, and the
/// warmed set still highlights. This is what the loop's ordering test will rest on.
#[test]
fn v022_syntax_warm_loads_once_and_reports() {
    // The premise: nothing has loaded the set yet (this is a fresh process, one test).
    assert!(
        !oxa_tui::syntaxes_loaded(),
        "the syntax set must load lazily, not at startup"
    );

    assert!(
        oxa_tui::warm_syntaxes(),
        "the first warm call must do the load and report it"
    );
    assert!(oxa_tui::syntaxes_loaded(), "the set is loaded after the warm-up");

    assert!(
        !oxa_tui::warm_syntaxes(),
        "a second warm call is a no-op and must not claim the load"
    );
    assert!(
        oxa_tui::syntaxes_loaded(),
        "the set stays loaded after the warm-up"
    );

    // The warm-up loaded the same, bundled set: a highlight on it still produces lines.
    let lines = oxa_tui::highlight_code("fn main() {}", Some("rust"), &oxa_tui::Theme::plain());
    assert!(!lines.is_empty(), "highlighting works on the warmed set");
}
