use oxa_orchestrator::cli::{maybe_run, parse};

fn args(extra: &[&str]) -> Vec<String> {
    [
        "--engine",
        "127.0.0.1:12345",
        "--model",
        "synthetic",
        "--local-only",
        "--directory",
        "unused-output",
    ]
    .into_iter()
    .chain(extra.iter().copied())
    .map(str::to_owned)
    .collect()
}

#[test]
fn help_and_unrelated_commands_are_inert() {
    assert!(parse(&["--help".into()]).unwrap().is_none());
    assert!(maybe_run(&[]).is_none());
    assert!(maybe_run(&["--mode".into(), "rpc".into()]).is_none());
}

#[test]
fn a_goal_after_the_delimiter_is_never_an_option() {
    assert!(parse(&args(&["--", "--help"])).unwrap().is_some());
    assert!(parse(&args(&["--", "!touch /tmp/not-a-command"]))
        .unwrap()
        .is_some());
}

#[test]
fn routes_and_runtime_options_fail_closed() {
    let mut missing = args(&["goal"]);
    missing.retain(|a| a != "--local-only");
    assert!(parse(&missing).is_err());
    let mut remote = args(&["goal"]);
    remote[1] = "192.0.2.1:8092".into();
    assert!(parse(&remote).is_err());
    for option in ["--yolo", "--online", "--api-key", "--worker-binary"] {
        assert!(parse(&args(&[option, "synthetic", "goal"])).is_err());
    }
}

#[test]
fn malformed_counts_roles_and_oversized_goals_are_rejected() {
    for extra in [
        vec!["--workers", "0", "goal"],
        vec!["--parallel", "17", "goal"],
        vec!["--workers", "1", "--parallel", "2", "goal"],
        vec!["--role", "one", "--role", "two", "--workers", "3", "goal"],
        vec!["--model", "other", "goal"],
    ] {
        assert!(parse(&args(&extra)).is_err());
    }
    let mut large = args(&[]);
    large.push("x".repeat(16385));
    assert!(parse(&large).is_err());
    assert!(parse(&args(&[])).is_err());
    assert!(parse(&args(&["--response-bytes", "16777216", "goal"]))
        .unwrap()
        .is_some());
}
