//! K2: the launch plan of a worker, a child `oxa` process that runs one goal in its own git
//! worktree. Pure functions only: nothing here spawns a process or touches the filesystem; the
//! caller runs the argv it gets back.
//!
//! * **Permission ladder.** plan < ask < workspace (standard) < auto < yolo. A worker never runs
//!   wider than its parent: a requested mode above the parent's ceiling is clamped to it and a
//!   notice is returned. `yolo` is passed only when the parent runs in yolo and `yolo` was asked
//!   for explicitly; a worker without a requested mode runs in `workspace`.
//! * **Worktrees.** `git -c core.hooksPath=/dev/null worktree add -b agent/<name>
//!   <repo-parent>/.oxa-worktrees/<repo>/<name> <base>`: hooks are off for the checkout.
//! * **Child env.** `OXA_WORKER_DEPTH=1`; the parent's `OXA_TRUST_PROJECT` and `OXA_SESSION_HINT`
//!   are not inherited.
use oxa_proto::Mode;
use std::path::Path;

/// A permission level a worker can run at, narrowest first (the derived order is the ladder).
/// `Ask` is only a parent ceiling: a worker is never requested to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkerMode {
    Plan,
    Ask,
    Workspace,
    Auto,
    Yolo,
}

impl WorkerMode {
    /// The value of `--permission-mode` for this level.
    pub fn as_arg(self) -> &'static str {
        match self {
            WorkerMode::Plan => "plan",
            WorkerMode::Ask => "ask",
            WorkerMode::Workspace => "workspace",
            WorkerMode::Auto => "auto",
            WorkerMode::Yolo => "yolo",
        }
    }
}

/// What the caller asks for one worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSpec {
    /// Validated by [`valid_name`].
    pub name: String,
    pub session_id: String,
    pub model: String,
    /// Path of the goal file, passed to `--goal`.
    pub goal_file: String,
    /// The requested mode; `None` means `workspace`. `Some(WorkerMode::Yolo)` is the only way to
    /// ask for yolo.
    pub mode: Option<WorkerMode>,
}

/// The name rule: `[a-z0-9][a-z0-9-]{0,39}`.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    match bytes.split_first() {
        Some((first, rest)) => {
            bytes.len() <= 40
                && (first.is_ascii_lowercase() || first.is_ascii_digit())
                && rest
                    .iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        }
        None => false,
    }
}

/// The widest level a worker of a parent in `parent_mode` may run at. `plan` denies writes
/// whatever the autonomy; a yolo parent (`parent_yolo`) allows yolo.
fn ceiling(parent_mode: Mode, parent_yolo: bool) -> WorkerMode {
    match parent_mode {
        Mode::Plan => WorkerMode::Plan,
        _ if parent_yolo => WorkerMode::Yolo,
        Mode::Ask => WorkerMode::Ask,
        Mode::Workspace => WorkerMode::Workspace,
    }
}

/// A value passed as its own argv element: it must not be empty, must not look like a flag, and
/// must not hold control characters.
fn check_value(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.starts_with('-') || value.chars().any(char::is_control) {
        return Err(format!("invalid worker {label}: {value:?}"));
    }
    Ok(())
}

/// The argv of the child (without the program): `--mode json --session-id <id> --model <m>
/// --permission-mode <p> --goal <file> -n <name>`, and a notice when the requested mode was
/// clamped to the parent's ceiling.
pub fn child_argv(
    spec: &WorkerSpec,
    parent_mode: Mode,
    parent_yolo: bool,
) -> Result<(Vec<String>, Option<String>), String> {
    if !valid_name(&spec.name) {
        return Err(format!("invalid worker name: {:?}", spec.name));
    }
    check_value("session id", &spec.session_id)?;
    check_value("model", &spec.model)?;
    check_value("goal file", &spec.goal_file)?;
    let wanted = spec.mode.unwrap_or(WorkerMode::Workspace);
    let ceil = ceiling(parent_mode, parent_yolo);
    let mode = wanted.min(ceil);
    let notice = (mode < wanted).then(|| {
        format!(
            "worker {}: permission mode {} is wider than this session's {}; using {}",
            spec.name,
            wanted.as_arg(),
            ceil.as_arg(),
            mode.as_arg()
        )
    });
    let args = [
        "--mode",
        "json",
        "--session-id",
        spec.session_id.as_str(),
        "--model",
        spec.model.as_str(),
        "--permission-mode",
        mode.as_arg(),
        "--goal",
        spec.goal_file.as_str(),
        "-n",
        spec.name.as_str(),
    ]
    .map(str::to_string)
    .to_vec();
    Ok((args, notice))
}

/// The git commands that create a worker's worktree, one argv each (program first). Hooks are
/// disabled for the checkout. Returns no command, which starts nothing, when the name is not
/// valid, the base is empty or looks like a flag, or the repo path has no UTF-8 name.
pub fn worktree_argv(repo: &Path, name: &str, base: &str) -> Vec<Vec<String>> {
    let (Some(repo_name), Some(parent)) = (repo.file_name().and_then(|n| n.to_str()), repo.parent()) else {
        return vec![];
    };
    if !valid_name(name) || base.is_empty() || base.starts_with('-') {
        return vec![];
    }
    let Some(path) = parent
        .join(".oxa-worktrees")
        .join(repo_name)
        .join(name)
        .to_str()
        .map(str::to_string)
    else {
        return vec![];
    };
    vec![vec![
        "git".into(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "worktree".into(),
        "add".into(),
        "-b".into(),
        format!("agent/{name}"),
        path,
        base.into(),
    ]]
}

/// The environment of a worker: the parent's, without the trust and session-hint variables, and
/// with `OXA_WORKER_DEPTH=1`.
pub fn child_env(parent_env: &[(String, String)]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = parent_env
        .iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "OXA_TRUST_PROJECT" | "OXA_SESSION_HINT" | "OXA_WORKER_DEPTH"
            )
        })
        .cloned()
        .collect();
    env.push(("OXA_WORKER_DEPTH".into(), "1".into()));
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(mode: Option<WorkerMode>) -> WorkerSpec {
        WorkerSpec {
            name: "k2-slice".into(),
            session_id: "sess-1".into(),
            model: "gpt-x".into(),
            goal_file: "/work/goal.md".into(),
            mode,
        }
    }

    /// The value after `--permission-mode` in `args`.
    fn perm(args: &[String]) -> &str {
        let i = args
            .iter()
            .position(|a| a == "--permission-mode")
            .expect("permission flag");
        &args[i + 1]
    }

    #[test]
    fn argv_never_widens_permission_mode() {
        // Plan parent: nothing wider than plan, whatever the autonomy.
        let (args, notice) = child_argv(&spec(Some(WorkerMode::Auto)), Mode::Plan, true).unwrap();
        assert_eq!(perm(&args), "plan");
        assert!(notice.is_some());
        // Ask parent: a workspace request is clamped to ask.
        let (args, notice) = child_argv(&spec(Some(WorkerMode::Workspace)), Mode::Ask, false).unwrap();
        assert_eq!(perm(&args), "ask");
        assert!(notice.is_some());
        // Workspace parent without yolo: yolo is never passed, even when asked for.
        let (args, notice) = child_argv(&spec(Some(WorkerMode::Yolo)), Mode::Workspace, false).unwrap();
        assert_eq!(perm(&args), "workspace");
        assert!(!args.iter().any(|a| a == "yolo"));
        assert!(notice.is_some());
        // No requested mode: workspace, no notice, never yolo.
        let (args, notice) = child_argv(&spec(None), Mode::Workspace, true).unwrap();
        assert_eq!(perm(&args), "workspace");
        assert!(notice.is_none());
        // Yolo parent with yolo asked for explicitly: yolo, no notice.
        let (args, notice) = child_argv(&spec(Some(WorkerMode::Yolo)), Mode::Workspace, true).unwrap();
        assert_eq!(perm(&args), "yolo");
        assert!(notice.is_none());
        // Yolo parent, plan table mode: still plan.
        let (args, _) = child_argv(&spec(Some(WorkerMode::Yolo)), Mode::Plan, true).unwrap();
        assert_eq!(perm(&args), "plan");
    }

    #[test]
    fn argv_has_session_id_goal_model_and_json_mode() {
        let (args, notice) = child_argv(&spec(None), Mode::Workspace, false).unwrap();
        assert_eq!(
            args,
            [
                "--mode",
                "json",
                "--session-id",
                "sess-1",
                "--model",
                "gpt-x",
                "--permission-mode",
                "workspace",
                "--goal",
                "/work/goal.md",
                "-n",
                "k2-slice",
            ]
        );
        assert!(notice.is_none());
        // Invalid name, and values that would read as flags, are refused.
        let mut bad = spec(None);
        bad.name = "Bad_Name".into();
        assert!(child_argv(&bad, Mode::Workspace, false).is_err());
        let mut flag = spec(None);
        flag.model = "--yolo".into();
        assert!(child_argv(&flag, Mode::Workspace, false).is_err());
    }

    #[test]
    fn worktree_argv_disables_git_hooks_and_uses_agent_branch() {
        let argv = worktree_argv(Path::new("/home/u/Work/ox-llm"), "k2-slice", "origin/main");
        assert_eq!(
            argv,
            vec![vec![
                "git",
                "-c",
                "core.hooksPath=/dev/null",
                "worktree",
                "add",
                "-b",
                "agent/k2-slice",
                "/home/u/Work/.oxa-worktrees/ox-llm/k2-slice",
                "origin/main",
            ]]
        );
        // Invalid name or a base that reads as a flag starts nothing.
        assert!(worktree_argv(Path::new("/home/u/Work/ox-llm"), "Bad Name", "origin/main").is_empty());
        assert!(worktree_argv(Path::new("/home/u/Work/ox-llm"), "k2-slice", "--force").is_empty());
    }

    #[test]
    fn child_env_drops_trust_and_session_hint() {
        let parent = vec![
            ("OXA_TRUST_PROJECT".to_string(), "1".to_string()),
            ("OXA_SESSION_HINT".to_string(), "abc.s1".to_string()),
            ("HOME".to_string(), "/home/u".to_string()),
        ];
        assert_eq!(
            child_env(&parent),
            vec![
                ("HOME".to_string(), "/home/u".to_string()),
                ("OXA_WORKER_DEPTH".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn name_rule_is_lowercase_digits_and_dashes() {
        assert!(valid_name("a"));
        assert!(valid_name("0-worker"));
        assert!(valid_name(&"a".repeat(40)));
        assert!(!valid_name(&"a".repeat(41)));
        assert!(!valid_name("-leading"));
        assert!(!valid_name("UPPER"));
        assert!(!valid_name("under_score"));
        assert!(!valid_name(""));
    }
}
