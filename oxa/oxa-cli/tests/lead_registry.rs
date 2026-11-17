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
        let output = command(&test).args(&args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("lead"));
        assert_eq!(text.contains("lead serve"), args.as_slice() == ["lead"]);
    }
    for child in ["board", "status", "asks", "chat"] {
        let output = command(&test).args(["lead", child]).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unavailable"));
    }
}

#[test]
fn offline_menu_precedes_home_and_runtime() {
    let test = TestEnv::new();
    for args in [vec!["--offline", "lead"], vec!["--offline", "--offline", "lead"]] {
        let output = command(&test).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Open the lead command menu"));
    }
}

#[test]
fn legacy_shadow_keeps_its_read_only_resource_floor() {
    for text in [
        "lead --shadow task",
        "lead --model synthetic --shadow task",
        "lead task --shadow",
    ] {
        let words = text.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        let args = oxa_cli::parse_args(&words, &[]).unwrap();
        assert!(args.lead && args.shadow);
        assert!(args.no_tools && args.no_mcp && args.no_extensions && args.no_web_search);
        assert_eq!(args.tools.unwrap(), ["read", "grep", "find", "ls"]);
    }
}
