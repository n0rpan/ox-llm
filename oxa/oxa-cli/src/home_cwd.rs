//! Item 3 (sec-prod): a session started in `$HOME`, `/` or a parent of `$HOME`. The bash tool
//! then sees the whole home directory (minus the deny-list) with network on, so the start is
//! refused unless the user opted in. Interactive mode warns and asks (default No); print, json
//! and rpc refuse. Opt-in: `oxa.allowHomeCwd: true` in the USER settings, or `OXA_ALLOW_HOME_CWD`
//! set to a truthy value. Subcommands and `--version` / `--help` / `--list-models` start no
//! session and never reach this check.
use crate::dispatch::RunMode;
use oxa_config::{Dirs, Env, Settings};
use oxa_proto::{codes, OxaError, PermissionMode};
use oxa_sandbox::{is_home_itself, is_home_or_above};
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
    /// `--yolo` or `--auto` on the command line: warn and go on, in every mode, without asking.
    Warn,
    /// Interactive: show the warning and ask (default No).
    Confirm,
    /// Print, json, rpc: refuse.
    Refuse,
}

/// The permission flag on the command line that counts as the owner's opt-in to a home start.
/// `--standard`, `--plan` and the config default do not.
pub(crate) fn explicit_opt_in(level: Option<PermissionMode>) -> Option<PermissionMode> {
    level.filter(|l| matches!(l, PermissionMode::Yolo | PermissionMode::Auto))
}

/// The decision for `mode`, whether `cwd` is a home root, whether the user opted in in settings
/// or the environment, and whether the command line named `--yolo` or `--auto`.
pub(crate) fn decide(mode: RunMode, home_root: bool, opted_in: bool, explicit: bool) -> Start {
    if !home_root || opted_in {
        return Start::Proceed;
    }
    if explicit {
        return Start::Warn;
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

/// O-199: an interactive start in the home directory itself that the user confirmed (or opted
/// into with `oxa.allowHomeCwd` / `OXA_ALLOW_HOME_CWD`) makes the home a workspace for bash
/// (`oxa_sandbox::Layout::allow_home_write`). Never for `/` or a parent of the home, and
/// never in print, json or rpc mode (only the explicit `oxa.sandbox.allowHomeWrite` setting
/// does that there, so a script cannot widen access by accident).
pub(crate) fn home_workspace(mode: RunMode, start: Start, home_itself: bool) -> bool {
    mode == RunMode::Interactive
        && home_itself
        && matches!(start, Start::Proceed | Start::Warn | Start::Confirm)
}

/// What the start check decided besides stopping: whether the home is the bash workspace, and
/// the warning to show (only for an explicit `--yolo` / `--auto`).
#[derive(Debug, Default)]
pub(crate) struct HomeStart {
    pub workspace: bool,
    pub warning: Option<String>,
}

/// The start check: `Ok(HomeStart)` to go on, `Err` (one line, non-zero exit through `report`)
/// to stop. `explicit` is the `--yolo` / `--auto` from the command line (see
/// [`explicit_opt_in`]). Interactive mode prints the question on `stdout` and reads the answer
/// from `stdin`.
pub(crate) fn check(
    mode: RunMode,
    cwd: &Path,
    env: &Env,
    dirs: &Dirs,
    explicit: Option<PermissionMode>,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
) -> Result<HomeStart, OxaError> {
    // A symlink to the home directory is the home directory: check the resolved path as well.
    let resolved = cwd.canonicalize();
    let home_root =
        is_home_or_above(cwd, &dirs.home) || resolved.is_ok_and(|real| is_home_or_above(&real, &dirs.home));
    if !home_root {
        return Ok(HomeStart::default());
    }
    let home_itself = is_home_itself(cwd, &dirs.home);
    let user = Settings::load(dirs, cwd, false);
    let opted_in =
        crate::netguard::truthy(env.get(ENV_ALLOW_HOME_CWD)) || user.bool(SETTING_ALLOW_HOME_CWD, false);
    let start = decide(mode, home_root, opted_in, explicit.is_some());
    match start {
        Start::Proceed => Ok(HomeStart {
            workspace: home_workspace(mode, start, home_itself),
            warning: None,
        }),
        Start::Warn => Ok(HomeStart {
            workspace: home_workspace(mode, start, home_itself),
            warning: explicit.map(|level| warning(cwd, level)),
        }),
        Start::Refuse => Err(refusal(cwd)),
        Start::Confirm => {
            let e = refusal(cwd);
            let effect = if home_itself {
                crate::sandbox_layout::home_confirm_effect()
            } else {
                "If you go on, bash gets no writable directory here.".to_string()
            };
            let _ = writeln!(
                stdout,
                "Warning: {}\n{}\n{}",
                e.message,
                e.hint.clone().unwrap_or_default(),
                effect
            );
            let _ = write!(stdout, "Start oxa here anyway? [y/N] ");
            let _ = stdout.flush();
            let answer = read_answer(stdin);
            let answer = answer.trim().to_lowercase();
            if answer == "y" || answer == "yes" {
                Ok(HomeStart {
                    workspace: home_workspace(mode, start, home_itself),
                    warning: None,
                })
            } else {
                Err(e)
            }
        }
    }
}

/// The warning for an explicit `--yolo` / `--auto` start in a home root (one line, shown with the
/// other start notices of the session).
fn warning(cwd: &Path, level: PermissionMode) -> String {
    format!(
        "oxa was started in {}, which is your home directory or one of its parents; --{} on the command line allows it, so oxa goes on without asking",
        cwd.display(),
        level.as_str()
    )
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
            assert_eq!(decide(mode, false, false, false), Start::Proceed);
        }
        // A home root, not opted in: interactive asks, the others refuse.
        assert_eq!(decide(RunMode::Interactive, true, false, false), Start::Confirm);
        assert_eq!(decide(RunMode::Print, true, false, false), Start::Refuse);
        assert_eq!(decide(RunMode::Json, true, false, false), Start::Refuse);
        assert_eq!(decide(RunMode::Rpc, true, false, false), Start::Refuse);
        // Opted in: go on in every mode.
        for mode in [RunMode::Interactive, RunMode::Print, RunMode::Json, RunMode::Rpc] {
            assert_eq!(decide(mode, true, true, false), Start::Proceed);
        }
    }

    #[test]
    fn home_cwd_yolo_flag_warns_only() {
        // S11: an explicit --yolo / --auto is the opt-in. It warns in every mode and never asks
        // or refuses, so `cd ~ && oxa --yolo -p ...` works.
        for mode in [RunMode::Interactive, RunMode::Print, RunMode::Json, RunMode::Rpc] {
            assert_eq!(decide(mode, true, false, true), Start::Warn);
        }
        assert_eq!(
            explicit_opt_in(Some(PermissionMode::Yolo)),
            Some(PermissionMode::Yolo)
        );
        assert_eq!(
            explicit_opt_in(Some(PermissionMode::Auto)),
            Some(PermissionMode::Auto)
        );
        // --standard, --plan and the configured default are not an opt-in.
        assert_eq!(explicit_opt_in(Some(PermissionMode::Standard)), None);
        assert_eq!(explicit_opt_in(Some(PermissionMode::Plan)), None);
        assert_eq!(explicit_opt_in(None), None);
        // The warning does not change where bash may write: only an interactive start in home
        // itself makes home the workspace, as for a confirmed start.
        assert!(home_workspace(RunMode::Interactive, Start::Warn, true));
        assert!(!home_workspace(RunMode::Print, Start::Warn, true));
        let text = warning(Path::new("/home/u"), PermissionMode::Yolo);
        assert!(text.contains("--yolo on the command line"), "{text}");
        assert!(!text.contains('\n'));
    }

    #[test]
    fn only_an_interactive_start_in_home_itself_is_a_workspace() {
        // O-199: confirmed (or opted in) in $HOME itself, interactive: a workspace.
        assert!(home_workspace(RunMode::Interactive, Start::Confirm, true));
        assert!(home_workspace(RunMode::Interactive, Start::Proceed, true));
        // A parent of $HOME or `/`: never.
        assert!(!home_workspace(RunMode::Interactive, Start::Confirm, false));
        assert!(!home_workspace(RunMode::Interactive, Start::Proceed, false));
        // Print, json, rpc: never through the start check (only the explicit setting).
        for mode in [RunMode::Print, RunMode::Json, RunMode::Rpc] {
            assert!(!home_workspace(mode, Start::Proceed, true));
            assert!(!home_workspace(mode, Start::Refuse, true));
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
