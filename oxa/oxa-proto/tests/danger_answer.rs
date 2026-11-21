//! #1512 S2: the dangerous-command prompt is defaulted on `Ui`, so a `Ui` that cannot ask
//! (print, JSON, tests) denies it and never consents by accident.
use oxa_proto::{Access, DangerAnswer, HeadlessUi, Ui};
use std::path::PathBuf;

#[test]
fn headless_ui_ask_dangerous_defaults_to_deny() {
    let mut ui = HeadlessUi::default();
    let a = Access::Exec {
        command: "git push --force origin main".into(),
        net: false,
        background: false,
    };
    assert_eq!(
        ui.ask_dangerous(&a, &["git-push-force: force".into()], &[]),
        DangerAnswer::Deny
    );
    let write = Access::Write(PathBuf::from("/w/a.rs"));
    assert_eq!(ui.ask_dangerous(&write, &[], &[]), DangerAnswer::Deny);
}
