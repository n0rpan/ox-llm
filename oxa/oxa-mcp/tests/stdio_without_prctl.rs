//! S2b: MCP stdio off Linux. There is no PR_SET_PDEATHSIG call on these platforms; the stdio
//! server command still builds, spawns and is stopped by the parent's `close`. Linux keeps the
//! parent-death signal and is covered by e7_mcp_core_and_stdio_transport.rs.
#![cfg(not(target_os = "linux"))]

use oxa_fake::TestEnv;
use oxa_mcp::*;

#[test]
fn stdio_command_builds_and_runs_without_prctl() {
    let env = TestEnv::new();
    let marker = env.work().join("ran");
    let cfg = StdioConfig {
        command: "sh".into(),
        args: vec!["-c".into(), format!("touch '{}'", marker.display())],
        ..Default::default()
    };
    let mut transport = StdioTransport::spawn(&cfg, &[], &env.work(), &env.home())
        .expect("the stdio server starts without a parent-death signal");
    // `close` waits for the short-lived server to exit, so its `touch` has run by now.
    transport.close();
    assert!(marker.exists(), "the stdio server ran");
}
