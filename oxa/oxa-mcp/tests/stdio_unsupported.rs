//! Parent-death supervision is required; an unsupported child must never execute.
#![cfg(not(target_os = "linux"))]

#[test]
fn stdio_child_is_refused_before_exec() {
    let env = oxa_fake::TestEnv::new();
    let marker = env.work().join("must-not-exist");
    let cfg = oxa_mcp::StdioConfig {
        command: "sh".into(),
        args: vec!["-c".into(), format!("touch '{}'", marker.display())],
        ..Default::default()
    };
    let result = oxa_mcp::StdioTransport::spawn(&cfg, &[], &env.work(), &env.home());
    assert!(matches!(result, Err(e) if e.to_string().contains("only on Linux")));
    assert!(!marker.exists());
}
