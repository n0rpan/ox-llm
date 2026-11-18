//! K5b: resume of workers when a parent session is opened again. Pure planning only: which
//! records are relaunched and with which argv. The spawn goes through K5a's launch path and the
//! K4 supervisor (`WorkerCore::resume` in `tool.rs`); nothing here runs a process or builds a
//! shell command string, and the child id is the one in the registry record, never model text.
//!
//! A record is relaunched when its state is [`WorkerState::Dead`]: not finished, not stopped by
//! the owner, and its process is gone (for example the parent shut down and stopped the group).
use super::launch::{check_value, resume_mode, WorkerMode};
use super::registry::{WorkerRecord, WorkerState};
use super::tool::parse_mode;
use oxa_proto::Autonomy;

/// Setting: relaunch dead, unfinished workers when the parent session opens (default true).
pub const SETTING_RESUME: &str = "oxa.workers.resumeOnStart";

/// Whether `state` is one that gets relaunched.
pub fn due(state: &WorkerState) -> bool {
    matches!(state, WorkerState::Dead)
}

/// The argv of the relaunched child (without the program): `--mode json --continue=<id> --model <m>
/// --permission-mode <p>`. There is no `--goal`: the session carries its goal. The permission
/// mode is the one recorded at start, never wider than the parent's autonomy allows now; a notice
/// is returned when it was narrowed.
pub fn resume_argv(
    rec: &WorkerRecord,
    parent_autonomy: Autonomy,
) -> Result<(Vec<String>, Option<String>), String> {
    check_value("session id", &rec.session_id)?;
    check_value("model", &rec.model)?;
    let recorded: WorkerMode =
        parse_mode(&rec.mode).ok_or_else(|| format!("unknown recorded mode {:?}", rec.mode))?;
    let mode = resume_mode(recorded, parent_autonomy);
    let notice = (mode < recorded).then(|| {
        format!(
            "worker {}: permission mode {} is wider than this session's; resuming with {}",
            rec.name,
            recorded.as_arg(),
            mode.as_arg()
        )
    });
    let args = vec![
        "--mode".to_string(),
        "json".to_string(),
        format!("--continue={}", rec.session_id),
        "--model".to_string(),
        rec.model.clone(),
        "--permission-mode".to_string(),
        mode.as_arg().to_string(),
    ];
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
        }
    }

    #[test]
    fn only_dead_workers_are_due() {
        assert!(due(&WorkerState::Dead));
        assert!(!due(&WorkerState::Stopped));
        assert!(!due(&WorkerState::Idle));
        assert!(!due(&WorkerState::Finished("done".into())));
    }

    #[test]
    fn resume_argv_continues_without_goal_and_never_widens() {
        let (args, notice) = resume_argv(&rec("workspace"), Autonomy::Standard).unwrap();
        assert!(args.iter().any(|a| a == "--continue=p-a-1"));
        assert!(!args.iter().any(|a| a == "--goal"));
        assert!(notice.is_none());
        let (args, notice) = resume_argv(&rec("yolo"), Autonomy::Standard).unwrap();
        assert!(args.windows(2).any(|w| w == ["--permission-mode", "workspace"]));
        assert!(notice.is_some());
        assert!(resume_argv(&rec("bogus"), Autonomy::Standard).is_err());
    }
}
