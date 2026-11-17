//! Pure mode selection and argument-error classification.
use crate::{parse_args, Args, ModeArg};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Interactive,
    Print,
    Json,
    Rpc,
}

/// cli.md's tty rules (see the crate docs). Pure.
pub fn choose_mode(a: &Args, stdin_is_tty: bool, stdout_is_tty: bool) -> RunMode {
    match a.mode {
        Some(ModeArg::Rpc) => RunMode::Rpc,
        Some(ModeArg::Json) => RunMode::Json,
        _ if a.print || !stdin_is_tty || !stdout_is_tty => RunMode::Print,
        _ => RunMode::Interactive,
    }
}

pub(super) fn headless_argument_error(argv: &[String], stdin_tty: bool, stdout_tty: bool) -> bool {
    let standalone = parse_args(&argv[..argv.len().min(1)], &[])
        .ok()
        .is_some_and(|args| args.subcommand.is_some());
    let flags: Vec<&str> = argv
        .iter()
        .map(String::as_str)
        .take_while(|arg| *arg != "--")
        .collect();
    let single_action = flags.iter().any(|arg| {
        ["--export", "--list-models", "--help", "-h", "--version", "-v"]
            .iter()
            .any(|flag| *arg == *flag || arg.starts_with(&format!("{flag}=")))
    });
    if standalone || single_action {
        return false;
    }
    !stdin_tty
        || !stdout_tty
        || flags
            .iter()
            .any(|arg| matches!(*arg, "-p" | "--print" | "--mode=rpc" | "--mode=json"))
        || flags
            .windows(2)
            .any(|pair| pair[0] == "--mode" && matches!(pair[1], "rpc" | "json"))
}
