//! Unsupported inbox operations fail explicitly instead of pretending to deliver messages.
#![cfg(not(target_os = "linux"))]

#[test]
fn inbox_is_explicitly_unsupported() {
    let env = oxa_fake::TestEnv::new();
    let error = oxa_session::send_inbox(&env.agent_dir(), "session-test", "synthetic").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    let result = oxa_session::Inbox::watch(&env.agent_dir(), "session-test", |_| panic!("delivery"));
    assert!(matches!(result, Err(e) if e.kind() == std::io::ErrorKind::Unsupported));
    assert!(!env.agent_dir().join("inbox").exists());
}
