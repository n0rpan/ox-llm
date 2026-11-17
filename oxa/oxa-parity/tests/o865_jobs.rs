//! #865: `/jobs` and the running-job HUD count in the real TUI (the real `oxa` binary in a
//! pty against the fake engine). Not PARITY rows: pi has no `/jobs`.
//! Build the binaries first: `cargo build -p oxa-cli -p oxa-fake`.
#![cfg(target_os = "linux")]

#[path = "support/tui_rig.rs"]
mod rig;

use oxa_fake::{ScriptTurn, TestEnv};
use rig::{Rig, RigOpts};

/// A background job started by the model: the HUD counts it, `/jobs` lists it with its last
/// output line, `/jobs stop <id>` ends it and the count goes away.
#[test]
fn jobs_lists_stops_and_counts_in_the_hud() {
    let env = TestEnv::new();
    let script = vec![
        ScriptTurn::tool(
            "c1",
            "bash",
            r#"{"command":"echo JOBLINE-865; sleep 60","background":true}"#,
        ),
        ScriptTurn::text("job started"),
    ];
    let rig = Rig::start_in(
        env,
        script,
        RigOpts {
            size: (30, 140),
            // The HUD is OXA's own look; the shared rig drives pi's.
            env: vec![("OXA_SKIN".to_string(), "oxa".to_string())],
            ..RigOpts::default()
        },
    );
    // The OXA skin's own status bar (its hint line drops the exit hint at this width).
    rig.pty.wait_for_text("OXA │").unwrap();
    assert!(
        !rig.screen().contains("JOBS"),
        "no jobs at the start: {:?}",
        rig.screen()
    );

    rig.prompt("start a long job", "job started");
    // The HUD counts the running job while the session is idle.
    let s = rig.pty.wait_for_text("JOBS 1").unwrap();
    assert!(s.contains(" OXA │ "), "the HUD is the OXA one: {s:?}");

    // `/jobs` lists it: the id, the status and the last line the job wrote.
    rig.slash("/jobs");
    let s = rig.pty.wait_for_text("Background jobs (1):").unwrap();
    assert!(s.contains("job-1"), "{s:?}");
    assert!(s.contains("running"), "{s:?}");
    assert!(s.contains("JOBLINE-865"), "{s:?}");

    // `/jobs stop <id>` ends it and the HUD count goes away.
    rig.slash("/jobs stop job-1");
    rig.pty.wait_for_text("Stopped job-1 (killed).").unwrap();
    rig.pty.wait_for_text_gone("JOBS 1").unwrap();

    // Stopping it again says it had already finished; an unknown id is an error notice.
    rig.slash("/jobs stop job-1");
    rig.pty.wait_for_text("had already finished (killed).").unwrap();
    rig.slash("/jobs stop job-9");
    rig.pty.wait_for_text("Unknown job: job-9").unwrap();
}
