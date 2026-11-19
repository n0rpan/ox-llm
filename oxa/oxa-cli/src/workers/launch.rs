//! K2: the launch plan of a worker, a child `oxa` process that runs one goal in its own git
//! worktree. Pure functions only: nothing here spawns a process or touches the filesystem; the
//! caller runs the argv it gets back.
//!
//! * **Permission ladder.** plan < ask < workspace (standard) < auto < yolo. A worker never runs
//!   wider than its parent: a requested mode above the parent's ceiling is clamped to it and a
//!   notice is returned. A worker without a requested mode inherits its parent's ceiling (yolo
//!   parent, yolo worker; auto, auto; workspace, workspace; plan, plan); an explicit narrower
//!   mode still wins.
//! * **Worktrees.** `git -c core.hooksPath=/dev/null worktree add --no-checkout -b agent/<name>
//!   <repo-parent>/.oxa-worktrees/<repo>/<name> <base>`: metadata only on the host, so no hook,
//!   smudge filter or `.gitattributes` callback of the repository runs outside the sandbox. The
//!   caller materializes the files inside the sandbox (`oxa work --supervise`, checkout).
//! * **Child env.** `OXA_WORKER_DEPTH=1`; the parent's `OXA_TRUST_PROJECT` and `OXA_SESSION_HINT`
//!   are not inherited.
//! * **Integration boundary (K4).** `child_env` copies whatever GitHub token variables the caller
//!   puts in the parent environment. A runner-owned packet must go through the existing worker
//!   spawn's credential removal and authority marker (`restrict_child_github` in `oxa-work`);
//!   this module does not strip them.
use oxa_proto::{Autonomy, Mode};
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
    /// The requested mode; `None` means the parent's ceiling. An explicit narrower mode wins;
    /// a wider one is clamped to the ceiling.
    pub mode: Option<WorkerMode>,
    /// The parent's token allowance (`--max-tokens`): the child may not spend more than it.
    pub max_tokens: Option<u64>,
    /// The parent's explicit tool exclusions (`--no-bash`, `-xt`): the child keeps them.
    pub exclude_tools: Vec<String>,
    /// The parent's resolved session directory, when it is not the default one.
    pub session_dir: Option<std::path::PathBuf>,
    /// The parent's other narrowing switches (`--offline`, `--tools`, `-nt`, `-nbt`, `--no-mcp`,
    /// `--no-web-search`, `-ne`): a worker is never wider than its parent.
    pub narrowing: Narrowing,
}

/// The parent's narrowing command-line switches, as `args.rs` parses them. Every one reaches the
/// worker; a new narrowing flag in `args.rs` must be added here too (`worker_inherits_every_narrowing_flag`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Narrowing {
    pub offline: bool,
    /// An explicit `--no-approve` must not fall back to project auto-trust in the child.
    pub no_approve: bool,
    /// `--no-context-files`: do not rediscover repository instructions in the worker checkout.
    pub no_context_files: bool,
    /// `--tools <list>`: the allowlist.
    pub tools: Option<Vec<String>>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    pub no_mcp: bool,
    pub no_web_search: bool,
    pub no_extensions: bool,
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

/// The widest level a worker of a parent running in table mode `parent_mode` with autonomy
/// `parent_autonomy` may run at. The table mode and the autonomy are separate fields of the
/// session: a workspace table with auto autonomy is the effective `auto` mode (as
/// `PermissionMode::of` shows it), so its ceiling is `Auto`, not `Workspace`. `plan` denies
/// writes whatever the autonomy.
fn ceiling(parent_mode: Mode, parent_autonomy: Autonomy) -> WorkerMode {
    match (parent_mode, parent_autonomy) {
        (Mode::Plan, _) => WorkerMode::Plan,
        (_, Autonomy::Yolo) => WorkerMode::Yolo,
        (_, Autonomy::Auto) => WorkerMode::Auto,
        (Mode::Ask, Autonomy::Standard) => WorkerMode::Ask,
        (Mode::Workspace, Autonomy::Standard) => WorkerMode::Workspace,
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
/// clamped to the parent's ceiling. The parent is given as its table mode and its autonomy (the
/// two fields a session keeps), not as a yolo flag.
pub fn child_argv(
    spec: &WorkerSpec,
    parent_mode: Mode,
    parent_autonomy: Autonomy,
) -> Result<(Vec<String>, Option<String>), String> {
    if !valid_name(&spec.name) {
        return Err(format!("invalid worker name: {:?}", spec.name));
    }
    check_value("session id", &spec.session_id)?;
    check_value("model", &spec.model)?;
    check_value("goal file", &spec.goal_file)?;
    let ceil = ceiling(parent_mode, parent_autonomy);
    let wanted = spec.mode.unwrap_or(ceil);
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
    let mut args = [
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
    // The parent's own limits and scope reach the child, so starting a worker is not a way
    // around them.
    if let Some(tokens) = spec.max_tokens {
        args.push(format!("--max-tokens={tokens}"));
    }
    if !spec.exclude_tools.is_empty() {
        let excluded: Vec<&str> = spec.exclude_tools.iter().map(String::as_str).collect();
        check_value("excluded tool", &excluded.join(","))?;
        args.push(format!("--exclude-tools={}", excluded.join(",")));
    }
    let n = &spec.narrowing;
    if let Some(tools) = &n.tools {
        let allowed: Vec<&str> = tools.iter().map(String::as_str).collect();
        for tool in &allowed {
            // `--tools=<list>` is one argument, so a `+name` / `-name` patch entry (CLI-027)
            // cannot read as a flag; the name itself still may not start with a dash.
            check_value("allowed tool", tool.strip_prefix(['+', '-']).unwrap_or(tool))?;
        }
        args.push(format!("--tools={}", allowed.join(",")));
    }
    for (on, flag) in [
        (n.offline, "--offline"),
        (n.no_approve, "--no-approve"),
        (n.no_context_files, "--no-context-files"),
        (n.no_tools, "--no-tools"),
        (n.no_builtin_tools, "--no-builtin-tools"),
        (n.no_mcp, "--no-mcp"),
        (n.no_web_search, "--no-web-search"),
        (n.no_extensions, "--no-extensions"),
    ] {
        if on {
            args.push(flag.to_string());
        }
    }
    if let Some(dir) = &spec.session_dir {
        check_value("session directory", &dir.to_string_lossy())?;
        args.push(format!("--session-dir={}", dir.display()));
    }
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
        "--no-checkout".into(),
        "-b".into(),
        format!("agent/{name}"),
        path,
        base.into(),
    ]]
}

/// The environment of a worker: the parent's, without the trust and session-hint variables, and
/// with `OXA_WORKER_DEPTH=1`. `offline` is the parent's own policy (`--offline` sets it without
/// any variable): the child is told, so it cannot reach the network behind the parent's back.
pub fn child_env(parent_env: &[(String, String)], offline: bool) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = parent_env
        .iter()
        .map(|(k, v)| {
            if k == "OXA_OFFLINE" && offline {
                (k.clone(), "1".to_string())
            } else {
                (k.clone(), v.clone())
            }
        })
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "OXA_TRUST_PROJECT" | "OXA_SESSION_HINT" | "OXA_WORKER_DEPTH"
            )
        })
        .collect();
    if offline && !env.iter().any(|(k, _)| k == "OXA_OFFLINE") {
        env.push(("OXA_OFFLINE".into(), "1".into()));
    }
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
            max_tokens: None,
            exclude_tools: Vec::new(),
            session_dir: None,
            narrowing: Narrowing::default(),
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
        let (args, notice) = child_argv(&spec(Some(WorkerMode::Auto)), Mode::Plan, Autonomy::Auto).unwrap();
        assert_eq!(perm(&args), "plan");
        assert!(notice.is_some());
        // Ask parent: a workspace request is clamped to ask.
        let (args, notice) =
            child_argv(&spec(Some(WorkerMode::Workspace)), Mode::Ask, Autonomy::Standard).unwrap();
        assert_eq!(perm(&args), "ask");
        assert!(notice.is_some());
        // Workspace parent without yolo: yolo is never passed, even when asked for.
        let (args, notice) =
            child_argv(&spec(Some(WorkerMode::Yolo)), Mode::Workspace, Autonomy::Standard).unwrap();
        assert_eq!(perm(&args), "workspace");
        assert!(!args.iter().any(|a| a == "yolo"));
        assert!(notice.is_some());
        // No requested mode under a yolo parent: yolo, no notice.
        let (args, notice) = child_argv(&spec(None), Mode::Workspace, Autonomy::Yolo).unwrap();
        assert_eq!(perm(&args), "yolo");
        assert!(notice.is_none());
        // An explicit narrower mode under yolo still wins.
        let (args, notice) = child_argv(
            &spec(Some(WorkerMode::Workspace)),
            Mode::Workspace,
            Autonomy::Yolo,
        )
        .unwrap();
        assert_eq!(perm(&args), "workspace");
        assert!(notice.is_none());
        // No requested mode inherits the other ceilings too.
        let (args, _) = child_argv(&spec(None), Mode::Workspace, Autonomy::Standard).unwrap();
        assert_eq!(perm(&args), "workspace");
        let (args, _) = child_argv(&spec(None), Mode::Plan, Autonomy::Yolo).unwrap();
        assert_eq!(perm(&args), "plan");
        // Yolo parent with yolo asked for explicitly: yolo, no notice.
        let (args, notice) =
            child_argv(&spec(Some(WorkerMode::Yolo)), Mode::Workspace, Autonomy::Yolo).unwrap();
        assert_eq!(perm(&args), "yolo");
        assert!(notice.is_none());
        // Yolo parent, plan table mode: still plan.
        let (args, _) = child_argv(&spec(Some(WorkerMode::Yolo)), Mode::Plan, Autonomy::Yolo).unwrap();
        assert_eq!(perm(&args), "plan");
    }

    #[test]
    fn auto_parent_keeps_auto_worker_not_workspace() {
        // A workspace table with auto autonomy is the effective `auto` mode: an auto child is
        // passed as auto (not clamped to workspace, which the CLI reads as standard prompts).
        let (auto_args, notice) =
            child_argv(&spec(Some(WorkerMode::Auto)), Mode::Workspace, Autonomy::Auto).unwrap();
        assert_eq!(perm(&auto_args), "auto");
        assert!(notice.is_none());
        // The same effective mode with an ask table: auto as well.
        let (args, notice) = child_argv(&spec(Some(WorkerMode::Auto)), Mode::Ask, Autonomy::Auto).unwrap();
        assert_eq!(perm(&args), "auto");
        assert!(notice.is_none());
        // An auto parent never widens to yolo, even when yolo is asked for.
        let (args, notice) =
            child_argv(&spec(Some(WorkerMode::Yolo)), Mode::Workspace, Autonomy::Auto).unwrap();
        assert_eq!(perm(&args), "auto");
        assert!(!args.iter().any(|a| a == "yolo"));
        assert!(notice.is_some());
        // An auto parent with no requested mode inherits auto.
        let (args, notice) = child_argv(&spec(None), Mode::Workspace, Autonomy::Auto).unwrap();
        assert_eq!(perm(&args), "auto");
        assert!(notice.is_none());
        // The argv value is read back by the real CLI parser as auto, not as standard prompts.
        let pair = ["--permission-mode".to_string(), perm(&auto_args).to_string()];
        let parsed = crate::parse_args(&pair, &[]).unwrap();
        assert_eq!(parsed.level, Some(oxa_proto::PermissionMode::Auto));
        // The workspace child above is the standard-prompts case, and it still reads that way.
        let pair = ["--permission-mode".to_string(), "workspace".to_string()];
        let parsed = crate::parse_args(&pair, &[]).unwrap();
        assert_eq!(parsed.level, Some(oxa_proto::PermissionMode::Standard));
    }

    #[test]
    fn ask_table_with_standard_autonomy_keeps_ask_ceiling() {
        // The effective mode of ask+standard is standard, but its ceiling stays ask: a
        // workspace request is clamped to ask, not widened to workspace.
        let (args, notice) =
            child_argv(&spec(Some(WorkerMode::Workspace)), Mode::Ask, Autonomy::Standard).unwrap();
        assert_eq!(perm(&args), "ask");
        assert!(notice.is_some());
    }

    #[test]
    fn argv_has_session_id_goal_model_and_json_mode() {
        let (args, notice) = child_argv(&spec(None), Mode::Workspace, Autonomy::Standard).unwrap();
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
        assert!(child_argv(&bad, Mode::Workspace, Autonomy::Standard).is_err());
        let mut flag = spec(None);
        flag.model = "--yolo".into();
        assert!(child_argv(&flag, Mode::Workspace, Autonomy::Standard).is_err());
    }

    #[test]
    fn child_argv_carries_the_parent_limits_and_scope() {
        let mut spec = spec(None);
        spec.max_tokens = Some(5000);
        spec.exclude_tools = vec!["bash".into(), "web_search".into()];
        spec.session_dir = Some(std::path::PathBuf::from("/work/sessions"));
        let (args, _) = child_argv(&spec, Mode::Workspace, Autonomy::Standard).unwrap();
        for expected in [
            "--max-tokens=5000",
            "--exclude-tools=bash,web_search",
            "--session-dir=/work/sessions",
        ] {
            assert!(args.iter().any(|a| a == expected), "{expected}: {args:?}");
        }
        // A value that would read as a flag is refused rather than forwarded.
        let mut bad = spec.clone();
        bad.exclude_tools = vec!["--yolo".into()];
        assert!(child_argv(&bad, Mode::Workspace, Autonomy::Standard).is_err());
    }

    #[test]
    fn worker_inherits_every_narrowing_flag() {
        let mut spec = spec(None);
        spec.narrowing = Narrowing {
            offline: true,
            no_approve: true,
            no_context_files: true,
            tools: Some(vec!["read".into(), "grep".into()]),
            no_tools: true,
            no_builtin_tools: true,
            no_mcp: true,
            no_web_search: true,
            no_extensions: true,
        };
        let (args, _) = child_argv(&spec, Mode::Workspace, Autonomy::Standard).unwrap();
        for expected in [
            "--offline",
            "--no-approve",
            "--no-context-files",
            "--tools=read,grep",
            "--no-tools",
            "--no-builtin-tools",
            "--no-mcp",
            "--no-web-search",
            "--no-extensions",
        ] {
            assert!(args.iter().any(|a| a == expected), "{expected}: {args:?}");
        }
        // The child's own parser accepts every forwarded flag.
        let mut argv = vec!["oxa".to_string()];
        argv.extend(args.iter().cloned());
        assert!(crate::args::parse_args(&argv, &[]).is_ok(), "{argv:?}");
        let parsed = crate::args::parse_args(&argv, &[]).unwrap();
        assert!(parsed.offline && parsed.no_tools && parsed.no_builtin_tools);
        assert_eq!(parsed.approve, Some(false));
        assert!(parsed.no_mcp && parsed.no_web_search && parsed.no_extensions);
        assert_eq!(parsed.tools, Some(vec!["read".to_string(), "grep".to_string()]));
        // No narrowing flag, none forwarded.
        let (plain, _) = child_argv(&self::spec(None), Mode::Workspace, Autonomy::Standard).unwrap();
        assert!(!plain
            .iter()
            .any(|a| a.starts_with("--no-") || a == "--offline" || a.starts_with("--tools")));
        // A `-name` / `+name` patch selection is forwarded in the one-argument form.
        spec.narrowing.tools = Some(vec!["-ls".into(), "+codemode".into()]);
        let (patched, _) = child_argv(&spec, Mode::Workspace, Autonomy::Standard).expect("patch selection");
        assert!(
            patched.contains(&"--tools=-ls,+codemode".to_string()),
            "{patched:?}"
        );
        // A value that would read as a flag is refused.
        spec.narrowing.tools = Some(vec!["--yolo".into()]);
        assert!(child_argv(&spec, Mode::Workspace, Autonomy::Standard).is_err());
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
                "--no-checkout",
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
            child_env(&parent, false),
            vec![
                ("HOME".to_string(), "/home/u".to_string()),
                ("OXA_WORKER_DEPTH".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn child_env_carries_the_parent_offline_policy() {
        let parent = vec![("HOME".to_string(), "/home/u".to_string())];
        assert!(child_env(&parent, true)
            .iter()
            .any(|(k, v)| k == "OXA_OFFLINE" && v == "1"));
        assert!(!child_env(&parent, false).iter().any(|(k, _)| k == "OXA_OFFLINE"));
        // A value left in the parent environment cannot turn the policy off.
        let stale = vec![("OXA_OFFLINE".to_string(), "0".to_string())];
        assert!(child_env(&stale, true)
            .iter()
            .any(|(k, v)| k == "OXA_OFFLINE" && v == "1"));
        assert!(child_env(&stale, false)
            .iter()
            .any(|(k, v)| k == "OXA_OFFLINE" && v == "0"));
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
