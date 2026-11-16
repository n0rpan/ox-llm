//! The fake server's command-line help must not start a server or require a script.
use oxa_fake::TestEnv;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn run_cli(env: &TestEnv, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxa-fake"))
        .args(args)
        .env_clear()
        .envs(env.envs(""))
        .current_dir(env.work())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn oxa-fake");
    // Keep stdin open: help must exit on its own, not wait for the server's stdin EOF.
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("oxa-fake {args:?} did not exit: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn help_lists_options_without_starting_server() {
    let env = TestEnv::new();
    env.write("work/script.jsonl", "{\"text\":\"hello\"}\n");
    for flag in ["--help", "-h"] {
        for args in [
            vec![flag],
            vec!["--script", "missing.jsonl", flag],
            vec!["--script", "script.jsonl", flag],
        ] {
            let output = run_cli(&env, &args);
            assert!(output.status.success(), "{args:?}: {output:?}");
            assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
            let help = String::from_utf8(output.stdout).unwrap();
            assert!(help.contains("Usage: oxa-fake"), "{help}");
            assert!(!help.lines().any(|line| line.starts_with("listening ")), "{help}");
            for option in [
                "--script",
                "--record",
                "--port",
                "--n-ctx",
                "--model",
                "--done-gap-ms",
                "--hold-headers",
                "--comments-only",
                "--drop-at",
                "--hold-prefill",
                "--model-row",
                "--rewrite-resume-block",
                "--no-hints",
                "--help",
                "-h",
            ] {
                assert!(help.contains(option), "missing {option}: {help}");
            }
        }
    }
}
