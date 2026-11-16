//! Context-file budget warnings use the selected model's window, without blocking startup.
use oxa_fake::{FakeModel, FakeOpts, FakeServer, ScriptTurn, TestEnv};
use std::process::Command;

fn startup(args: &[&str], cap: u32, files: bool) -> String {
    let env = TestEnv::new();
    if files {
        env.write("home/.oxa/agent/AGENTS.md", &"a".repeat(1200));
        env.write("work/AGENTS.md", &"b".repeat(800));
    }
    let server = FakeServer::start(
        vec![ScriptTurn::text("ok")],
        FakeOpts {
            n_ctx: 8000,
            model: "small".into(),
            models: vec![FakeModel {
                alias: "selected".into(),
                cap,
                max_output: Some(1024),
                caps: None,
            }],
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_oxa"))
        .args([
            "--no-session",
            "--model",
            "selected",
            "--no-tools",
            "--no-skills",
            "--system-prompt",
            "Synthetic test assistant.",
        ])
        .args(args)
        .current_dir(env.work())
        .env_clear()
        .envs(env.envs(&server.url()))
        .output()
        .unwrap();
    let err = String::from_utf8(out.stderr).unwrap();
    // An RPC connection closed without a prompt has the normal NothingToDo outcome.
    let expected = if args.contains(&"rpc") { 4 } else { 0 };
    assert_eq!(out.status.code(), Some(expected), "{err}");
    err
}

#[test]
fn context_startup_warns_once_in_print_json_and_rpc() {
    for args in [
        vec!["-p", "hello"],
        vec!["--mode", "json", "hello"],
        vec!["--mode", "rpc"],
    ] {
        let err = startup(&args, 8000, true);
        assert_eq!(err.matches("context files use").count(), 1, "{err}");
        assert!(
            err.contains("~500 tokens (6.2% of selected's 8000-token window)"),
            "{err}"
        );
        assert!(
            err.contains("largest: ~/.oxa/agent/AGENTS.md (~300 tokens)"),
            "{err}"
        );
    }
}

#[test]
fn context_startup_is_quiet_at_or_below_five_percent_and_without_files() {
    for cap in [10_000, 20_000] {
        let err = startup(&["-p", "hello"], cap, true);
        assert!(!err.contains("context files use"), "{err}");
    }
    let err = startup(&["-p", "hello"], 8000, false);
    assert!(!err.contains("context files use"), "{err}");
    let err = startup(&["--no-context-files", "-p", "hello"], 8000, true);
    assert!(!err.contains("context files use"), "{err}");
}
