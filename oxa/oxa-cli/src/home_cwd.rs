//! Item 3 (sec-prod): a session started in `$HOME`, `/` or a parent of `$HOME`. The bash tool
//! then sees the whole home directory (minus the deny-list) with network on, so the start is
//! refused unless the user opted in. Interactive mode warns and asks (default No); print, json
//! and rpc refuse. Opt-in: `oxa.allowHomeCwd: true` in the USER settings, or `OXA_ALLOW_HOME_CWD`
//! set to a truthy value. Subcommands and `--version` / `--help` / `--list-models` start no
//! session and never reach this check.
use crate::dispatch::RunMode;
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{codes, OxaError};
use oxa_sandbox::is_home_or_above;
use std::io::{Read, Write};
use std::path::Path;

/// The user-settings key that allows a session in the home directory (read from the user file only).
pub(crate) const SETTING_ALLOW_HOME_CWD: &str = "oxa.allowHomeCwd";
/// The environment variable that allows it (pi's truthy values, as `OXA_OFFLINE`).
pub(crate) const ENV_ALLOW_HOME_CWD: &str = "OXA_ALLOW_HOME_CWD";

/// What the start does with a session in a home root. Pure decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Start {
    /// Not a home root, or opted in: start.
    Proceed,
    /// Interactive: show the warning and ask (default No).
    Confirm,
    /// Print, json, rpc: refuse.
    Refuse,
}

/// The decision for `mode`, whether `cwd` is a home root and whether the user opted in.
pub(crate) fn decide(mode: RunMode, home_root: bool, opted_in: bool) -> Start {
    if !home_root || opted_in {
        return Start::Proceed;
    }
    match mode {
        RunMode::Interactive => Start::Confirm,
        RunMode::Print | RunMode::Json | RunMode::Rpc => Start::Refuse,
    }
}

/// The one-line refusal (the warning of the interactive confirm has the same first line).
pub(crate) fn refusal(cwd: &Path) -> OxaError {
    OxaError::new(
        codes::USAGE,
        format!(
            "oxa was started in {}, which is your home directory or one of its parents; cd into a project directory first",
            cwd.display()
        ),
    )
    .with_hint(format!(
        "or set \"{SETTING_ALLOW_HOME_CWD}\": true in the user settings.json, or {ENV_ALLOW_HOME_CWD}=1, to allow it"
    ))
}

/// The start check: `Ok` to go on, `Err` (one line, non-zero exit through `report`) to stop.
/// Interactive mode prints the warning on `stdout` and reads the answer from `stdin`.
pub(crate) fn check(
    mode: RunMode,
    cwd: &Path,
    env: &Env,
    dirs: &Dirs,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
) -> Result<(), OxaError> {
    // A symlink to the home directory is the home directory: check the resolved path as well.
    let resolved = cwd.canonicalize();
    let home_root =
        is_home_or_above(cwd, &dirs.home) || resolved.is_ok_and(|real| is_home_or_above(&real, &dirs.home));
    if !home_root {
        return Ok(());
    }
    let user = Settings::load(dirs, cwd, false);
    let opted_in =
        crate::netguard::truthy(env.get(ENV_ALLOW_HOME_CWD)) || user.bool(SETTING_ALLOW_HOME_CWD, false);
    match decide(mode, home_root, opted_in) {
        Start::Proceed => Ok(()),
        Start::Refuse => Err(refusal(cwd)),
        Start::Confirm => {
            let e = refusal(cwd);
            let _ = writeln!(
                stdout,
                "Warning: {}\n{}",
                e.message,
                e.hint.clone().unwrap_or_default()
            );
            let _ = write!(stdout, "Start oxa here anyway? [y/N] ");
            let _ = stdout.flush();
            let answer = read_answer(stdin);
            let answer = answer.trim().to_lowercase();
            if answer == "y" || answer == "yes" {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// One line from `stdin`, read byte by byte (as the other prompts of `run`).
fn read_answer(stdin: &mut dyn Read) -> String {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = stdin.read(&mut byte) {
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
    }
    String::from_utf8_lossy(&line).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn decide_table() {
        // Not a home root: always go on.
        for mode in [RunMode::Interactive, RunMode::Print, RunMode::Json, RunMode::Rpc] {
            assert_eq!(decide(mode, false, false), Start::Proceed);
        }
        // A home root, not opted in: interactive asks, the others refuse.
        assert_eq!(decide(RunMode::Interactive, true, false), Start::Confirm);
        assert_eq!(decide(RunMode::Print, true, false), Start::Refuse);
        assert_eq!(decide(RunMode::Json, true, false), Start::Refuse);
        assert_eq!(decide(RunMode::Rpc, true, false), Start::Refuse);
        // Opted in: go on in every mode.
        for mode in [RunMode::Interactive, RunMode::Print, RunMode::Json, RunMode::Rpc] {
            assert_eq!(decide(mode, true, true), Start::Proceed);
        }
    }

    #[test]
    fn home_roots_are_home_its_parents_and_root() {
        let home = PathBuf::from("/home/u");
        assert!(is_home_or_above(&home, &home));
        assert!(is_home_or_above(Path::new("/home"), &home));
        assert!(is_home_or_above(Path::new("/"), &home));
        assert!(!is_home_or_above(&home.join("project"), &home));
        assert!(!is_home_or_above(Path::new("/home/other"), &home));
    }

    #[test]
    fn the_refusal_is_one_line_with_the_fix() {
        let e = refusal(Path::new("/home/u"));
        assert!(e
            .message
            .starts_with("oxa was started in /home/u, which is your home directory"));
        assert!(!e.message.contains('\n'));
        assert!(e
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("oxa.allowHomeCwd") && h.contains("OXA_ALLOW_HOME_CWD")));
    }
}
