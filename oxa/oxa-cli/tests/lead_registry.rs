//! The installed menu needs no runtime; reserved children stay unavailable.
use oxa_fake::TestEnv;
use std::process::Command;

fn command(test: &TestEnv) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_oxa"));
    command
        .env_clear()
        .envs(test.envs("http://127.0.0.1:1"))
        .env_remove("HOME")
        .current_dir(test.work());
    command
}

#[test]
fn menu_and_help_precede_runtime_and_reserved_children_refuse() {
    let test = TestEnv::new();
    for args in [vec!["lead"], vec!["lead", "--help"], vec!["help", "lead"]] {
        let output = command(&test).args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("lead"));
        assert!(!text.contains("lead serve"));
    }
    for child in ["serve", "board", "status", "asks", "chat"] {
        let output = command(&test).args(["lead", child]).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unavailable"));
    }
}
