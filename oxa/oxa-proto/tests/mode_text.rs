//! The permission-mode text never claims an OS sandbox that is not there.
use oxa_proto::PermissionMode;

#[test]
fn auto_mode_text_never_claims_a_sandbox_when_none_is_available() {
    for m in PermissionMode::ALL {
        let t = m.describe_with(false);
        if m != PermissionMode::Yolo && m != PermissionMode::Plan {
            assert!(t.contains("no OS sandbox"), "{m:?}: {t}");
        }
        // An operative claim, not the negation: "no OS sandbox on this platform" says the
        // opposite and must not be read as one.
        assert!(!t.contains("sandbox and deny-list on"), "{m:?}: {t}");
        assert!(!t.contains("the bash sandbox,"), "{m:?}: {t}");
    }
}

#[test]
fn mode_text_is_unchanged_when_the_sandbox_is_available() {
    assert_eq!(
        PermissionMode::Auto.describe_with(true),
        "no prompts; the bash sandbox, the deny-list and the protected paths stay on"
    );
    assert_eq!(
        PermissionMode::Standard.describe_with(true),
        "prompts before actions outside the workspace; sandbox and deny-list on"
    );
    #[cfg(target_os = "linux")]
    for m in PermissionMode::ALL {
        assert_eq!(m.describe(), m.describe_with(true));
    }
}

#[test]
fn start_notice_exists_exactly_when_there_is_no_os_sandbox() {
    assert_eq!(oxa_proto::no_os_sandbox_notice().is_none(), oxa_proto::OS_SANDBOX);
    #[cfg(target_os = "macos")]
    assert_eq!(
        oxa_proto::no_os_sandbox_notice(),
        Some("macOS: no OS sandbox; bash is disabled unless yolo")
    );
}
