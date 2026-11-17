//! V022 (#884): concurrent warm-ups still load once and report the load to exactly one caller.
//!
//! Separate binary from `v022_syntax_warm.rs`: `oxa_tui`'s syntax set is a process-wide
//! `OnceLock`, so each statement about "not loaded yet" needs its own process. The load here is
//! raced on purpose; add no second test to this file.

use std::sync::{Arc, Barrier};

/// `warm_syntaxes()` promises "true when *this* call did the load". A caller that lost the race
/// must not claim it, or a future call site that keys a one-shot action off the return value runs
/// it more than once.
#[test]
fn v022_syntax_warm_reports_the_load_to_exactly_one_caller() {
    assert!(
        !oxa_tui::syntaxes_loaded(),
        "the syntax set must load lazily, not at startup"
    );

    const CALLERS: usize = 8;
    let barrier = Arc::new(Barrier::new(CALLERS));
    let warmers: Vec<_> = (0..CALLERS)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                // Release every caller together so they all see the set unloaded.
                barrier.wait();
                oxa_tui::warm_syntaxes()
            })
        })
        .collect();
    let claims: Vec<bool> = warmers
        .into_iter()
        .map(|w| w.join().expect("warm-up thread panicked"))
        .collect();

    assert_eq!(
        claims.iter().filter(|claimed| **claimed).count(),
        1,
        "exactly one caller did the load: {claims:?}"
    );
    assert!(oxa_tui::syntaxes_loaded(), "the set is loaded after the warm-up");
}
