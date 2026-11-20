//! K5b: resume of workers when a parent session is opened again. Pure planning only: which
//! records are relaunched and with which argv. The spawn goes through K5a's launch path and the
//! K4 supervisor (`WorkerCore::resume` in `tool.rs`); nothing here runs a process or builds a
//! shell command string, and the child id is the one in the registry record, never model text.
//!
//! A record is relaunched when its process is gone, the owner did not stop it, and it did not
//! finish for good (see [`due`]): for example the parent shut down and stopped the group.
use super::launch::{check_value, resume_mode, scope_args, WorkerMode};
use super::registry::{WorkerRecord, WorkerState};
use super::tool::parse_mode;
use super::tool::ParentScope;
use oxa_proto::{Autonomy, Mode};

/// Setting: relaunch dead, unfinished workers when the parent session opens (default true).
pub const SETTING_RESUME: &str = "oxa.workers.resumeOnStart";

/// Whether the owner allows resume on open: `oxa.workers.resumeOnStart` from the user settings
/// only (default true). A project file is never read for it, so it can neither turn resume on
/// against the owner's `false` nor turn it off.
pub fn enabled(settings: &oxa_config::Settings) -> bool {
    settings.user_bool(SETTING_RESUME, true)
}

/// Whether a worker is relaunched: its process is gone (`alive` false) and either it never wrote a
/// final status ([`WorkerState::Dead`]) or its last status is `finished` with the outcome
/// `interrupted`. The second case is what a normal parent shutdown leaves: `Supervisor::stop_all`
/// sends SIGINT, the child ends cooperatively and records an interrupted finish while its goal is
/// still open. A worker the owner stopped (`rec.stopped`) is never relaunched, and any other
/// finished outcome (done, blocked, failed) is final.
pub fn due(rec: &WorkerRecord, state: &WorkerState, alive: bool) -> bool {
    if alive || rec.stopped {
        return false;
    }
    match state {
        WorkerState::Dead => true,
        WorkerState::Finished(outcome) => outcome == "interrupted",
        _ => false,
    }
}

/// The argv of the relaunched child (without the program): `--mode json --continue=<id> --model <m>
/// --permission-mode <p>` and the parent's token allowance, tool exclusions and session directory
/// (`scope`), as a first launch carries them. There is no `--goal`: the session carries its goal. The permission
/// mode is the one recorded at start, never wider than the parent's table mode and autonomy allow now; a notice
/// is returned when it was narrowed.
pub fn resume_argv(
    rec: &WorkerRecord,
    parent_mode: Mode,
    parent_autonomy: Autonomy,
    scope: &ParentScope,
) -> Result<(Vec<String>, Option<String>), String> {
    check_value("session id", &rec.session_id)?;
    check_value("model", &rec.model)?;
    let recorded: WorkerMode =
        parse_mode(&rec.mode).ok_or_else(|| format!("unknown recorded mode {:?}", rec.mode))?;
    let mode = resume_mode(recorded, parent_mode, parent_autonomy);
    let notice = (mode < recorded).then(|| {
        format!(
            "worker {}: permission mode {} is wider than this session's; resuming with {}",
            rec.name,
            recorded.as_arg(),
            mode.as_arg()
        )
    });
    let mut args = vec![
        "--mode".to_string(),
        "json".to_string(),
        format!("--continue={}", rec.session_id),
        "--model".to_string(),
        rec.model.clone(),
        "--permission-mode".to_string(),
        mode.as_arg().to_string(),
    ];
    args.extend(scope_args(
        scope.max_tokens,
        &scope.exclude_tools,
        scope.session_dir.as_deref(),
        &scope.narrowing,
    )?);
    Ok((args, notice))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(mode: &str) -> WorkerRecord {
        WorkerRecord {
            name: "a".into(),
            session_id: "p-a-1".into(),
            pid: 1,
            pgid: 1,
            worktree: "/w".into(),
            branch: "agent/a".into(),
            model: "m".into(),
            mode: mode.into(),
            goal_file: "/g".into(),
            status_file: "/s".into(),
            log_file: "/l".into(),
            started_ms: 0,
            stopped: false,
            ..WorkerRecord::default()
        }
    }

    #[test]
    fn interrupted_finish_after_parent_shutdown_is_resumed_unless_stopped() {
        let r = rec("workspace");
        let mut stopped = rec("workspace");
        stopped.stopped = true;
        let interrupted = WorkerState::Finished("interrupted".into());
        // Dead without a final status, and an interrupted finish: both come back.
        assert!(due(&r, &WorkerState::Dead, false));
        assert!(due(&r, &interrupted, false));
        // A final outcome, a live process, an owner stop and a working worker do not.
        assert!(!due(&r, &WorkerState::Finished("done".into()), false));
        assert!(!due(&r, &WorkerState::Finished("failed".into()), false));
        assert!(!due(&r, &interrupted, true));
        assert!(!due(&stopped, &WorkerState::Dead, false));
        assert!(!due(&stopped, &interrupted, false));
        assert!(!due(&r, &WorkerState::Stopped, false));
        assert!(!due(&r, &WorkerState::Idle, false));
    }

    #[test]
    fn resume_argv_continues_without_goal_and_never_widens() {
        let (args, notice) = resume_argv(
            &rec("workspace"),
            Mode::Workspace,
            Autonomy::Standard,
            &ParentScope::default(),
        )
        .unwrap();
        assert!(args.iter().any(|a| a == "--continue=p-a-1"));
        assert!(!args.iter().any(|a| a == "--goal"));
        assert!(notice.is_none());
        let (args, notice) = resume_argv(
            &rec("yolo"),
            Mode::Workspace,
            Autonomy::Standard,
            &ParentScope::default(),
        )
        .unwrap();
        assert!(args.windows(2).any(|w| w == ["--permission-mode", "workspace"]));
        assert!(notice.is_some());
        let scope = ParentScope {
            max_tokens: Some(900),
            exclude_tools: vec!["bash".into()],
            session_dir: Some("/work/sessions".into()),
            narrowing: super::super::launch::Narrowing {
                offline: true,
                no_context_files: true,
                tools: Some(vec!["read".into()]),
                no_tools: true,
                no_builtin_tools: true,
                no_mcp: true,
                no_web_search: true,
                no_extensions: true,
                no_approve: true,
                no_skills: true,
                no_prompt_templates: true,
            },
        };
        let (args, _) = resume_argv(&rec("workspace"), Mode::Workspace, Autonomy::Standard, &scope).unwrap();
        for expected in [
            "--max-tokens=900",
            "--exclude-tools=bash",
            "--session-dir=/work/sessions",
            "--offline",
            "--no-context-files",
            "--no-approve",
            "--no-skills",
            "--no-prompt-templates",
            "--tools=read",
            "--no-tools",
            "--no-builtin-tools",
            "--no-mcp",
            "--no-web-search",
            "--no-extensions",
        ] {
            assert!(args.iter().any(|a| a == expected), "{expected}: {args:?}");
        }
        assert!(resume_argv(
            &rec("bogus"),
            Mode::Workspace,
            Autonomy::Standard,
            &ParentScope::default()
        )
        .is_err());
    }

    fn mode_arg(args: &[String]) -> &str {
        let i = args.iter().position(|a| a == "--permission-mode").expect("flag");
        &args[i + 1]
    }

    #[test]
    fn plan_mode_parent_resumes_worker_at_plan() {
        for recorded in ["workspace", "auto", "yolo", "plan"] {
            for autonomy in [Autonomy::Standard, Autonomy::Auto, Autonomy::Yolo] {
                let (args, _) =
                    resume_argv(&rec(recorded), Mode::Plan, autonomy, &ParentScope::default()).unwrap();
                assert_eq!(mode_arg(&args), "plan", "{recorded} {autonomy:?}");
            }
        }
    }

    #[test]
    fn ask_mode_parent_never_widens_resumed_worker() {
        for recorded in ["workspace", "auto", "yolo"] {
            let (args, notice) = resume_argv(
                &rec(recorded),
                Mode::Ask,
                Autonomy::Standard,
                &ParentScope::default(),
            )
            .unwrap();
            assert_eq!(mode_arg(&args), "ask", "{recorded}");
            assert!(notice.is_some());
        }
        // A worker recorded narrower than the parent stays as recorded.
        let (args, _) = resume_argv(
            &rec("plan"),
            Mode::Ask,
            Autonomy::Standard,
            &ParentScope::default(),
        )
        .unwrap();
        assert_eq!(mode_arg(&args), "plan");
    }

    fn settings(user: serde_json::Value, project: serde_json::Value) -> oxa_config::Settings {
        oxa_config::Settings::from_values(user, project)
    }

    #[test]
    fn user_false_project_true_does_not_resume() {
        let off = serde_json::json!({"oxa": {"workers": {"resumeOnStart": false}}});
        let on = serde_json::json!({"oxa": {"workers": {"resumeOnStart": true}}});
        assert!(!enabled(&settings(off, on)));
    }

    #[test]
    fn project_false_cannot_disable_user_default() {
        let off = serde_json::json!({"oxa": {"workers": {"resumeOnStart": false}}});
        // Nothing set by the user: the default (true) holds, whatever the project says.
        assert!(enabled(&settings(serde_json::json!({}), off.clone())));
        assert!(enabled(&settings(serde_json::json!({}), serde_json::json!({}))));
        // An explicit user opt-in stays on.
        let on = serde_json::json!({"oxa": {"workers": {"resumeOnStart": true}}});
        assert!(enabled(&settings(on, off)));
    }
}
