//! #1512 S3: who is at the keyboard decides the OS sandbox.
//!
//! * [`RunKind::Unattended`]: no person watches the run (a subagent child, a lead worker, an
//!   `oxa work` run, a runner-owned task, a headless goal). The OS sandbox (bubblewrap) stays on
//!   by default, owner setting `oxa.sandbox.unattended`.
//! * [`RunKind::Attended`]: the TUI, a person's `oxa -p` and the lead chat. No OS sandbox by
//!   default; dangerous commands ask. `--sandbox` or `oxa.sandbox.interactive` turns it on.
//!
//! Every test of the decision reads the environment it is given (`Env`), never the process's.
use crate::args::Args;
use crate::child::ChildIdentity;
use crate::RunMode;
use oxa_config::{Env, Settings};

/// `OXA_WORKER_DEPTH`: set (to `1`) for the workers a lead starts.
pub(crate) const ENV_WORKER_DEPTH: &str = "OXA_WORKER_DEPTH";
/// The owner setting of the OS sandbox for an attended run (user settings only).
pub(crate) const SETTING_INTERACTIVE: &str = "oxa.sandbox.interactive";
/// The owner setting of the OS sandbox for an unattended run (user settings only).
pub(crate) const SETTING_UNATTENDED: &str = "oxa.sandbox.unattended";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunKind {
    Attended,
    Unattended,
}

/// Unattended when ANY of these holds (fail closed); everything else is attended: a subagent
/// child (`child.nested()`), `OXA_WORKER_DEPTH` (lead workers), `OXA_UNATTENDED=1` (every
/// spawner sets it), the worktree trust source of `oxa work`, `OXA_RUNNER_OWNS_GITHUB=1`
/// (runner-owned tasks), an ownerless RPC host, or a headless goal requested at startup.
/// Restored goals are checked after session open. RPC is attended only when its host
/// explicitly declares an owner.
pub(crate) fn run_kind(a: &Args, env: &Env, child: &ChildIdentity, mode: RunMode) -> RunKind {
    let unattended = child.nested()
        || env.get(ENV_WORKER_DEPTH).is_some()
        || env.get(oxa_proto::names::ENV_UNATTENDED) == Some("1")
        || env.get(oxa_work::WORKTREE_TRUST_SOURCE_ENV).is_some()
        || env.get(oxa_work::RUNNER_GITHUB_ENV) == Some("1")
        || (mode == RunMode::Rpc && env.get("OXA_RPC_OWNER_PRESENT") != Some("1"))
        || (a.goal.is_some() && mode != RunMode::Interactive);
    if unattended {
        RunKind::Unattended
    } else {
        RunKind::Attended
    }
}

/// Whether the OS sandbox (bwrap) is wanted for this run.
///
/// Decision table (each row is a named test):
///
/// | run                                        | OS sandbox | dangerous commands        |
/// |--------------------------------------------|------------|---------------------------|
/// | attended, default                          | no         | ask                       |
/// | attended, `--sandbox`                      | bwrap      | ask                       |
/// | unattended, default, bwrap present         | bwrap      | refused (S4)              |
/// | unattended, default, bwrap missing         | bash disabled (fail closed) | n/a      |
/// | unattended, `oxa.sandbox.unattended=false` | no         | approval request (S5)     |
/// | any run, yolo                              | no         | no prompts                |
/// | plan, bwrap present                        | bwrap (read-only policy) | n/a         |
/// | plan, bwrap missing                        | disabled (fail closed) | n/a |
///
/// `user` is the user layer only (`Settings::load(dirs, cwd, false)`): a project file never
/// sets either key. Plan mode is not a parameter: plan always delegates to the wrapped sandbox
/// and therefore fails closed when bwrap is missing.
pub(crate) fn os_sandbox_wanted(kind: RunKind, a: &Args, user: &Settings) -> bool {
    match kind {
        RunKind::Attended => a.sandbox || user.bool(SETTING_INTERACTIVE, false),
        RunKind::Unattended => unattended_os_sandbox(user),
    }
}

/// The owner’s unattended policy, including after a stored goal has been restored.
pub(crate) fn unattended_os_sandbox(settings: &Settings) -> bool {
    settings
        .user_value(SETTING_UNATTENDED)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// `oxa.sandbox.net = off` for commands without the OS sandbox: off in the user layer or in the
/// merged settings. A project file may turn the network off, never back on over the user's
/// `off` (without the OS sandbox this setting is the only network gate of bash).
pub(crate) fn sandbox_net_off(merged: &Settings, user: &Settings) -> bool {
    let on = |s: &Settings| {
        oxa_sandbox::sandbox_net_setting(s.string(oxa_sandbox::SETTING_SANDBOX_NET).as_deref())
    };
    !on(merged) || !on(user)
}

/// The warning for a missing bwrap: only when the run needs the OS sandbox (fail closed for
/// unattended runs). An attended run without it works without the warning.
pub(crate) fn bwrap_missing_notice(os_sandbox: bool, reason: &str) -> Option<String> {
    os_sandbox.then(|| format!("bwrap is missing or unusable ({reason}); bash is disabled"))
}

pub(crate) const NO_SANDBOX_NOTICE: &str =
    "No OS sandbox: bash runs with your rights; dangerous commands ask. \
     --sandbox or oxa.sandbox.interactive turns the sandbox on.";

/// The start notice of an attended run without the OS sandbox (yolo has its own banner).
pub(crate) fn no_sandbox_notice(kind: RunKind, os_sandbox: bool, yolo: bool) -> Option<&'static str> {
    (kind == RunKind::Attended && !os_sandbox && !yolo).then_some(NO_SANDBOX_NOTICE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxa_config::Dirs;
    use oxa_fake::TestEnv;
    use serde_json::json;

    fn no_env() -> Env {
        Env::from_pairs(&[])
    }

    fn user(value: serde_json::Value) -> Settings {
        Settings::from_values(value, json!({}))
    }

    fn args() -> Args {
        Args::default()
    }

    #[test]
    fn restored_goal_uses_the_owners_unattended_sandbox_setting() {
        let project_off = json!({"oxa": {"sandbox": {"unattended": false}}});
        assert!(unattended_os_sandbox(&Settings::from_values(
            json!({}),
            project_off
        )));
        let owner_off = json!({"oxa": {"sandbox": {"unattended": false}}});
        let project_on = json!({"oxa": {"sandbox": {"unattended": true}}});
        assert!(!unattended_os_sandbox(&Settings::from_values(
            owner_off, project_on
        )));
    }

    #[test]
    fn run_kind_table() {
        let none = ChildIdentity::default();
        let nested = ChildIdentity {
            depth: 1,
            ..ChildIdentity::default()
        };
        let goal = Args {
            goal: Some("goal.md".into()),
            ..Args::default()
        };
        let rows: Vec<(&str, Args, Env, ChildIdentity, RunMode, RunKind)> = vec![
            (
                "tui_attended",
                args(),
                no_env(),
                none.clone(),
                RunMode::Interactive,
                RunKind::Attended,
            ),
            (
                "print_by_person_attended",
                args(),
                no_env(),
                none.clone(),
                RunMode::Print,
                RunKind::Attended,
            ),
            (
                "lead_chat_attended",
                args(),
                no_env(),
                none.clone(),
                RunMode::Interactive,
                RunKind::Attended,
            ),
            (
                "subagent_depth_unattended",
                args(),
                no_env(),
                nested,
                RunMode::Print,
                RunKind::Unattended,
            ),
            (
                "worker_depth_unattended",
                args(),
                Env::from_pairs(&[(ENV_WORKER_DEPTH, "1")]),
                none.clone(),
                RunMode::Print,
                RunKind::Unattended,
            ),
            (
                "oxa_unattended_env_unattended",
                args(),
                Env::from_pairs(&[(oxa_proto::names::ENV_UNATTENDED, "1")]),
                none.clone(),
                RunMode::Print,
                RunKind::Unattended,
            ),
            (
                "worktree_trust_source_unattended",
                args(),
                Env::from_pairs(&[(oxa_work::WORKTREE_TRUST_SOURCE_ENV, "/w")]),
                none.clone(),
                RunMode::Json,
                RunKind::Unattended,
            ),
            (
                "runner_owns_github_unattended",
                args(),
                Env::from_pairs(&[(oxa_work::RUNNER_GITHUB_ENV, "1")]),
                none.clone(),
                RunMode::Print,
                RunKind::Unattended,
            ),
            (
                "ownerless_rpc_unattended",
                args(),
                no_env(),
                none.clone(),
                RunMode::Rpc,
                RunKind::Unattended,
            ),
            (
                "owner_present_rpc_attended",
                args(),
                Env::from_pairs(&[("OXA_RPC_OWNER_PRESENT", "1")]),
                none.clone(),
                RunMode::Rpc,
                RunKind::Attended,
            ),
            (
                "json_goal_unattended",
                goal.clone(),
                no_env(),
                none.clone(),
                RunMode::Json,
                RunKind::Unattended,
            ),
            (
                "interactive_goal_attended",
                goal,
                no_env(),
                none,
                RunMode::Interactive,
                RunKind::Attended,
            ),
        ];
        for (name, a, env, child, mode, want) in rows {
            assert_eq!(run_kind(&a, &env, &child, mode), want, "row {name}");
        }
    }

    #[test]
    fn os_sandbox_wanted_attended_default_is_off() {
        assert!(!os_sandbox_wanted(RunKind::Attended, &args(), &user(json!({}))));
    }

    #[test]
    fn os_sandbox_wanted_attended_flag_is_on() {
        let a = Args {
            sandbox: true,
            ..Args::default()
        };
        assert!(os_sandbox_wanted(RunKind::Attended, &a, &user(json!({}))));
        let setting = user(json!({"oxa": {"sandbox": {"interactive": true}}}));
        assert!(os_sandbox_wanted(RunKind::Attended, &args(), &setting));
    }

    #[test]
    fn os_sandbox_wanted_unattended_default_is_on() {
        assert!(os_sandbox_wanted(RunKind::Unattended, &args(), &user(json!({}))));
    }

    #[test]
    fn os_sandbox_wanted_unattended_setting_off_is_off() {
        let off = user(json!({"oxa": {"sandbox": {"unattended": false}}}));
        assert!(!os_sandbox_wanted(RunKind::Unattended, &args(), &off));
    }

    #[test]
    fn configured_os_sandbox_is_preserved_for_startup_yolo() {
        let a = Args {
            sandbox: true,
            ..Args::default()
        };
        assert!(os_sandbox_wanted(RunKind::Attended, &a, &user(json!({}))));
        assert!(os_sandbox_wanted(RunKind::Unattended, &args(), &user(json!({}))));
    }

    #[test]
    fn os_sandbox_wanted_unattended_no_bwrap_disables_bash() {
        let wanted = os_sandbox_wanted(RunKind::Unattended, &args(), &user(json!({})));
        assert!(wanted);
        let text = bwrap_missing_notice(wanted, "no bwrap on PATH").expect("warning");
        assert!(text.contains("bash is disabled"), "{text}");
    }

    #[test]
    fn bwrap_missing_attended_no_warning_bash_works() {
        let wanted = os_sandbox_wanted(RunKind::Attended, &args(), &user(json!({})));
        assert_eq!(bwrap_missing_notice(wanted, "no bwrap on PATH"), None);
    }

    #[test]
    fn bwrap_missing_unattended_bash_disabled() {
        let wanted = os_sandbox_wanted(RunKind::Unattended, &args(), &user(json!({})));
        let text = bwrap_missing_notice(wanted, "x").expect("warning");
        assert_eq!(text, "bwrap is missing or unusable (x); bash is disabled");
    }

    #[test]
    fn no_sandbox_notice_only_for_attended_runs_without_yolo() {
        assert!(no_sandbox_notice(RunKind::Attended, false, false).is_some());
        assert!(no_sandbox_notice(RunKind::Attended, true, false).is_none());
        assert!(no_sandbox_notice(RunKind::Attended, false, true).is_none());
        assert!(no_sandbox_notice(RunKind::Unattended, false, false).is_none());
    }

    #[test]
    fn project_settings_cannot_turn_sandbox_net_back_on() {
        let user_off = json!({"oxa": {"sandbox": {"net": "off"}}});
        let merged = Settings::from_values(user_off.clone(), json!({"oxa": {"sandbox": {"net": "on"}}}));
        assert!(
            sandbox_net_off(&merged, &user(user_off)),
            "a project cannot relax net = off"
        );
        let merged = Settings::from_values(json!({}), json!({"oxa": {"sandbox": {"net": "off"}}}));
        assert!(
            sandbox_net_off(&merged, &user(json!({}))),
            "a project may turn it off"
        );
        assert!(
            !sandbox_net_off(&user(json!({})), &user(json!({}))),
            "default is on"
        );
    }

    #[test]
    fn project_settings_cannot_set_sandbox_interactive_or_unattended() {
        let env = TestEnv::new();
        let dirs = Dirs::under(env.root());
        let cwd = env.work();
        std::fs::create_dir_all(&cwd).unwrap();
        let project = dirs.project_settings_file(&cwd);
        std::fs::create_dir_all(project.parent().unwrap()).unwrap();
        std::fs::write(
            &project,
            r#"{"oxa":{"sandbox":{"interactive":true,"unattended":false}}}"#,
        )
        .unwrap();
        // Untrusted project: the user layer alone decides.
        let settings = Settings::load(&dirs, &cwd, false);
        assert!(!os_sandbox_wanted(RunKind::Attended, &args(), &settings));
        assert!(os_sandbox_wanted(RunKind::Unattended, &args(), &settings));
    }
}
