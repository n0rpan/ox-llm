//! #784: the password path of a named root action. A fake `sudo` runs `$SUDO_ASKPASS` (the oxa
//! executable under its askpass name) and writes what it got to a file: the password typed into
//! the owner's dialog reaches the fake sudo, and appears nowhere a model could read it.
use oxa_core::owner_confirm::{OwnerConfirm, ASKPASS_NAME, SOCK_ENV};
use oxa_proto::{Access, Answer, Autonomy, Level, Placement, Ui, YoloSwitch};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::Duration;

struct Typed(String);

impl Ui for Typed {
    fn secret(&mut self, _: &str, _: &str) -> Option<String> {
        Some(self.0.clone())
    }
    fn confirm(&mut self, _: &str, _: &str, _: Option<u32>) -> Option<bool> {
        None
    }
    fn select(&mut self, _: &str, _: &[String], _: Option<u32>) -> Option<usize> {
        None
    }
    fn input(&mut self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn editor(&mut self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn notify(&mut self, _: &str, _: Level) {}
    fn set_status(&mut self, _: &str, _: Option<&str>) {}
    fn set_widget(&mut self, _: &str, _: Option<&[String]>, _: Placement) {}
    fn set_title(&mut self, _: &str) {}
    fn set_editor_text(&mut self, _: &str) {}
    fn ask_access(&mut self, _: &Access) -> Answer {
        Answer::Deny
    }
}

#[test]
fn the_password_reaches_sudo_and_nothing_else() {
    let password = "opaque-example-password";
    let dir = std::env::temp_dir().join(format!("oxa-askpass-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Yolo must not answer a password prompt: only the dialog does.
    let broker = OwnerConfirm::start(&dir.join("broker"), YoloSwitch::new(Autonomy::Yolo)).unwrap();
    broker.set_servable(true);
    // The fake sudo is no setuid binary: stand in for the peer check, keep the privileged-call window.
    broker.set_askpass_peer_check(|_| true);
    let _window = broker.open_askpass_window();
    let helper = dir.join(ASKPASS_NAME);
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_oxa"), &helper).unwrap();
    let fake_sudo = dir.join("sudo");
    std::fs::write(
        &fake_sudo,
        "#!/bin/sh\nprintf '%s' \"$(\"$SUDO_ASKPASS\" 'Password:')\" > \"$GOT\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let got = dir.join("got");

    let server = broker.clone();
    let owner = std::thread::spawn(move || {
        let mut ui = Typed(password.to_string());
        for _ in 0..500 {
            server.serve(&mut ui);
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let output = Command::new(&fake_sudo)
        .env("SUDO_ASKPASS", &helper)
        .env(SOCK_ENV, broker.sock_path())
        .env("GOT", &got)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read_to_string(&got).unwrap(), password);
    let shown = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !shown.contains(password),
        "the password is in the command's output: {shown}"
    );
    assert!(broker.take_records().is_empty());
    broker.close();
    drop(owner);
    let _ = std::fs::remove_dir_all(&dir);
}
