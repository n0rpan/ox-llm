//! Issue #104: the `done` setting. oxa runs the project's done commands itself when a run
//! that edited files is about to finish. Scripted engine, isolated `TestEnv`, real sandbox-less
//! shell in the work folder.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    Access, AgentEvent, CancelToken, CommandSpec, EventSink, ExecResult, ExecSpec, Exposure, HeadlessUi,
    HookPoint, HookReply, Hooks, OxaError, Sandbox, Source, Tool, ToolClass, ToolCtx, ToolOutput, ToolSpec,
    Ui,
};
use oxa_resources::Resources;
use oxa_session::MemStore;
use serde_json::json;
use std::sync::Arc;

/// Runs the command directly in the work folder (the real sandbox needs bwrap).
struct Direct;
impl Sandbox for Direct {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        oxa_sandbox::run_unsandboxed(s, c, live)
    }
}

#[derive(Default)]
struct Rec(Vec<AgentEvent>);
impl EventSink for Rec {
    fn emit(&mut self, e: &AgentEvent) {
        self.0.push(e.clone());
    }
}
impl Rec {
    fn checks(&self) -> Vec<String> {
        self.0
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Notice { text, .. } if text.starts_with("done check:") => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}

fn run(env: &TestEnv, done: serde_json::Value, turns: Vec<ScriptTurn>) -> (Rec, Arc<ScriptedEngine>) {
    run_with(env, done, turns, Arc::new(Direct))
}

fn run_with(
    env: &TestEnv,
    done: serde_json::Value,
    turns: Vec<ScriptTurn>,
    sandbox: Arc<dyn Sandbox>,
) -> (Rec, Arc<ScriptedEngine>) {
    let (rec, engine, _) = run_state(env, done, turns, sandbox);
    (rec, engine)
}

fn run_state(
    env: &TestEnv,
    done: serde_json::Value,
    turns: Vec<ScriptTurn>,
    sandbox: Arc<dyn Sandbox>,
) -> (Rec, Arc<ScriptedEngine>, bool) {
    let settings = if done.is_null() {
        Settings::default()
    } else {
        Settings::from_values(json!({ "done": done }), json!({}))
    };
    run_settings(env, settings, turns, sandbox)
}

fn run_settings(
    env: &TestEnv,
    settings: Settings,
    turns: Vec<ScriptTurn>,
    sandbox: Arc<dyn Sandbox>,
) -> (Rec, Arc<ScriptedEngine>, bool) {
    run_tools(env, settings, turns, sandbox, Vec::new())
}

fn run_tools(
    env: &TestEnv,
    settings: Settings,
    turns: Vec<ScriptTurn>,
    sandbox: Arc<dyn Sandbox>,
    extra: Vec<Arc<dyn Tool>>,
) -> (Rec, Arc<ScriptedEngine>, bool) {
    let engine = Arc::new(ScriptedEngine::new(turns));
    let tools = oxa_tools::builtin_tools();
    let hooks: Vec<Arc<dyn Hooks>> = if extra.is_empty() {
        Vec::new()
    } else {
        vec![Arc::new(ExtTools(extra))]
    };
    let deps = Deps {
        engine: engine.clone(),
        store: Arc::new(MemStore::new()),
        tools,
        hooks,
        policy: Box::new(ModePolicy::new()),
        sandbox,
        settings,
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    let mut s = Session::open(deps, Open::New).expect("open");
    let mut rec = Rec::default();
    s.prompt("go".into(), Vec::new(), &mut rec, &mut HeadlessUi::default())
        .expect("prompt");
    let failed = s.inner().stats.done_failed;
    (rec, engine, failed)
}

fn write(id: &str, path: &str) -> ScriptTurn {
    ScriptTurn::tool(id, "write", &json!({"path": path, "content": "x"}).to_string())
}

const CHECK: &str = "test -f fixed.txt || { echo MISSING-FIXED; exit 3; }";

#[test]
fn failing_check_feeds_model_then_passes() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!([CHECK]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            write("c2", "fixed.txt"),
            ScriptTurn::text("really done"),
        ],
    );
    let checks = rec.checks();
    assert_eq!(checks.len(), 2, "{checks:?}");
    assert!(
        checks[0].contains("FAILED (exit 3)") && checks[0].contains("MISSING-FIXED"),
        "{checks:?}"
    );
    assert!(checks[1].ends_with("ok"), "{checks:?}");
    let reqs = engine.requests();
    assert_eq!(reqs.len(), 4);
    let last_user = reqs[2]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == oxa_proto::Role::User)
        .unwrap();
    assert!(last_user.content.as_ref().unwrap().contains("MISSING-FIXED"));
    assert_eq!(engine.remaining(), 0);
}

#[test]
fn passing_check_adds_no_round() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!(["true"]),
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
    );
    assert_eq!(rec.checks(), vec!["done check: true ok".to_string()]);
    assert_eq!(engine.requests().len(), 2);
}

#[test]
fn no_edits_no_checks() {
    let env = TestEnv::new();
    let (rec, engine) = run(&env, json!(["exit 1"]), vec![ScriptTurn::text("nothing to do")]);
    assert!(rec.checks().is_empty());
    assert_eq!(engine.requests().len(), 1);
}

#[test]
fn off_by_default() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        serde_json::Value::Null,
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
    );
    assert!(rec.checks().is_empty());
    assert_eq!(engine.requests().len(), 2);
}

#[test]
fn failing_forever_stops_after_two_rounds() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!(["exit 1"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            write("c2", "b.txt"),
            ScriptTurn::text("done"),
            write("c3", "c.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("must not be requested"),
        ],
    );
    assert_eq!(rec.checks().len(), 3, "{:?}", rec.checks());
    assert_eq!(engine.requests().len(), 6);
    assert_eq!(engine.remaining(), 1);
}

/// A `done` entry whose program does not exist (the shell exits 127) says nothing about the
/// source: it is reported as not run with the shell's own reason, spends no repair round, and
/// the run still ends failed, so an unverified tree is never reported as a pass (#966).
#[test]
fn unstartable_check_is_reported_as_not_run_and_not_retried() {
    let env = TestEnv::new();
    let (rec, engine, failed) = run_state(
        &env,
        json!(["oxa-966-missing-program --check"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("must not be requested for a check that cannot start"),
        ],
        Arc::new(Direct),
    );
    let checks = rec.checks();
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert!(
        checks[0].starts_with("done check: oxa-966-missing-program --check FAILED (not run)"),
        "{checks:?}"
    );
    assert!(checks[0].contains("command not found"), "{checks:?}");
    assert!(failed, "a check that did not run is not a pass");
    assert_eq!(engine.requests().len(), 2, "no repair round: {checks:?}");
    assert_eq!(engine.remaining(), 1, "no repair round: {checks:?}");
}

/// A missing execute bit on a project script is repairable, not an unavailable toolchain.
#[test]
fn non_executable_project_check_is_repaired() {
    let env = TestEnv::new();
    std::fs::write(env.work().join("check.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    let (rec, engine, failed) = run_state(
        &env,
        json!(["./check.sh"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::tool("fix", "bash", r#"{"command":"chmod +x check.sh"}"#),
            ScriptTurn::text("repaired"),
        ],
        Arc::new(Direct),
    );
    let checks = rec.checks();
    assert_eq!(checks.len(), 2, "{checks:?}");
    assert!(checks[0].contains("FAILED (exit 126)"), "{checks:?}");
    assert!(checks[0].contains("Permission denied"), "{checks:?}");
    assert!(checks[1].ends_with("ok"), "{checks:?}");
    assert!(!failed, "repair must actually verify the source");
    assert_eq!(engine.requests().len(), 4);
    assert_eq!(engine.remaining(), 0);
}

/// A check that started can propagate either code from a repairable project helper.
#[test]
fn started_project_checks_with_126_or_127_keep_their_repair_round() {
    for code in [126, 127] {
        let env = TestEnv::new();
        std::fs::write(env.work().join("check.sh"), format!("exit {code}\n")).unwrap();
        let (rec, engine, failed) = run_state(
            &env,
            json!(["sh ./check.sh"]),
            vec![
                write("c1", "a.txt"),
                ScriptTurn::text("done"),
                ScriptTurn::tool("fix", "write", r#"{"path":"check.sh","content":"exit 0\n"}"#),
                ScriptTurn::text("repaired"),
            ],
            Arc::new(Direct),
        );
        let checks = rec.checks();
        assert_eq!(checks.len(), 2, "exit {code}: {checks:?}");
        assert!(checks[0].contains(&format!("FAILED (exit {code})")), "{checks:?}");
        assert!(checks[1].ends_with("ok"), "{checks:?}");
        assert!(!failed, "exit {code}: repaired source was verified");
        assert_eq!(engine.requests().len(), 4);
        assert_eq!(engine.remaining(), 0);
    }
}

/// An independently confirmed non-executable external command remains not run, once.
#[test]
fn non_executable_external_check_is_reported_as_not_run() {
    let env = TestEnv::new();
    let external = env.root().join("external-check.sh");
    std::fs::write(&external, "#!/bin/sh\nexit 0\n").unwrap();
    let entry = external.to_str().unwrap();
    let (rec, engine, failed) = run_state(
        &env,
        json!([entry]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("unused"),
        ],
        Arc::new(Direct),
    );
    let checks = rec.checks();
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert!(checks[0].contains("FAILED (not run)"), "{checks:?}");
    assert!(checks[0].contains("Permission denied"), "{checks:?}");
    assert!(failed);
    assert_eq!(engine.requests().len(), 2);
    assert_eq!(engine.remaining(), 1);
}

/// A real failure in the same list keeps its repair round; the entry that cannot start is not
/// repeated as a problem the model is asked to fix.
#[test]
fn real_failure_keeps_its_round_beside_an_unstartable_check() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!([CHECK, "oxa-966-missing-program"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            write("c2", "fixed.txt"),
            ScriptTurn::text("done again"),
        ],
    );
    assert_eq!(engine.requests().len(), 4);
    let reqs = engine.requests();
    let repair = reqs[2]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == oxa_proto::Role::User)
        .and_then(|m| m.content.clone())
        .expect("the real failure is sent back");
    assert!(repair.contains("MISSING-FIXED"), "{repair}");
    assert!(
        !repair.contains("oxa-966-missing-program"),
        "a command that cannot start is not a project problem to fix: {repair}"
    );
    let checks = rec.checks();
    assert!(
        checks.iter().any(|c| c.contains("FAILED (not run)")),
        "{checks:?}"
    );
}

#[test]
fn failed_check_without_new_edit_is_not_rerun() {
    let env = TestEnv::new();
    let (rec, engine) = run(
        &env,
        json!(["exit 1"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("cannot fix"),
        ],
    );
    assert_eq!(rec.checks().len(), 1);
    assert_eq!(engine.requests().len(), 3);
}

/// Records every command and cancels the token while the first one runs.
struct CancelDuringFirst(std::sync::Mutex<Vec<String>>);
impl Sandbox for CancelDuringFirst {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.0.lock().unwrap().push(s.command.clone());
        c.cancel();
        ExecResult {
            exit: None,
            ..Default::default()
        }
    }
}

#[test]
fn cancel_during_first_check_stops_done_phase() {
    let env = TestEnv::new();
    let spy = Arc::new(CancelDuringFirst(Default::default()));
    let (rec, engine) = run_with(
        &env,
        json!(["echo first", "echo second"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("must not be requested"),
        ],
        spy.clone(),
    );
    let ran = spy.0.lock().unwrap().clone();
    assert_eq!(ran.len(), 1, "{ran:?}");
    assert!(ran[0].contains("first"), "{ran:?}");
    assert!(rec.checks().is_empty(), "{:?}", rec.checks());
    assert_eq!(engine.requests().len(), 2);
    assert_eq!(engine.remaining(), 1);
}

/// Records every `ExecSpec` it receives, then runs it directly.
struct Recording {
    specs: std::sync::Mutex<Vec<ExecSpec>>,
}
impl Sandbox for Recording {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        self.specs.lock().unwrap().push(s.clone());
        oxa_sandbox::run_unsandboxed(s, c, live)
    }
}

/// Recording `run` that first swaps the validated child folder for a symlink to an outside
/// directory, then delegates to Direct. Used as the deterministic child-replacement negative
/// control.
struct SwapChild {
    specs: std::sync::Mutex<Vec<ExecSpec>>,
    work: std::path::PathBuf,
    outside: std::path::PathBuf,
}
impl Sandbox for SwapChild {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        // Between validation and use, re-point `sub` at an outside directory.
        std::fs::remove_dir_all(self.work.join("sub")).expect("remove sub");
        std::os::unix::fs::symlink(&self.outside, self.work.join("sub")).expect("swap sub");
        self.specs.lock().unwrap().push(s.clone());
        oxa_sandbox::run_unsandboxed(s, c, live)
    }
}

/// Runs one `done` entry after a `write`; returns the check notices.
fn one(env: &TestEnv, entry: &str) -> Vec<String> {
    let (rec, _) = run(
        env,
        json!([entry]),
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
    );
    rec.checks()
}

#[test]
fn in_dir_runs_the_command_in_that_folder() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("sub/deep")).unwrap();
    std::fs::write(env.work().join("sub/marker.txt"), "x").unwrap();
    // The marker exists in `sub` only, so the check passes only with cwd = sub.
    assert_eq!(
        one(&env, "in sub: test -f marker.txt"),
        vec!["done check: [sub] test -f marker.txt ok".to_string()]
    );
    // `./sub/deep` style and an absolute path inside the session folder work too.
    assert_eq!(
        one(&env, "in ./sub/deep/..: test -f marker.txt")[0],
        "done check: [./sub/deep/..] test -f marker.txt ok"
    );
    let abs = env.work().join("sub");
    let entry = format!("in {}: test -f marker.txt", abs.display());
    assert!(one(&env, &entry)[0].ends_with("ok"), "{:?}", one(&env, &entry));
    // Without the prefix the command still runs in the session folder.
    let plain = one(&env, "test -f marker.txt");
    assert!(plain[0].contains("FAILED (exit 1)"), "{plain:?}");
}

#[test]
fn in_dir_failure_names_the_folder() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("sub")).unwrap();
    let c = one(&env, "in sub: echo BOOM; exit 4");
    assert!(
        c[0].starts_with("done check: [sub] echo BOOM; exit 4 FAILED (exit 4)") && c[0].contains("BOOM"),
        "{c:?}"
    );
}

#[test]
fn in_dir_refuses_missing_and_escaping_folders() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("sub")).unwrap();
    // A sibling of the work folder exists, so a refusal is about the escape, not about it missing.
    std::fs::create_dir_all(env.root().join("outside")).unwrap();
    let marker = env.root().join("outside/ran.txt");
    let outside_abs = env.root().join("outside");
    let cases = [
        ("in nope: touch ran.txt".to_string(), "does not exist"),
        (
            "in ../outside: touch ran.txt".to_string(),
            "outside the project folder",
        ),
        (
            "in sub/../../outside: touch ran.txt".to_string(),
            "outside the project folder",
        ),
        (
            format!("in {}: touch ran.txt", outside_abs.display()),
            "outside the project folder",
        ),
        (
            "in ../nope: touch ran.txt".to_string(),
            "outside the project folder",
        ),
        ("in : touch ran.txt".to_string(), "no folder"),
    ];
    for (entry, reason) in cases {
        let c = one(&env, &entry);
        assert_eq!(c.len(), 1, "{entry}: {c:?}");
        assert!(
            c[0].contains("FAILED (not run)") && c[0].contains(reason),
            "{entry}: {c:?}"
        );
    }
    assert!(!marker.exists(), "a refused check must not run");
    // A symlink that leaves the session folder is refused as well.
    std::os::unix::fs::symlink(env.root().join("outside"), env.work().join("link")).unwrap();
    let c = one(&env, "in link: touch ran.txt");
    assert!(
        c[0].contains("FAILED (not run)") && c[0].contains("outside"),
        "{c:?}"
    );
    assert!(!marker.exists());
    // A file is not a folder.
    std::fs::write(env.work().join("file.txt"), "x").unwrap();
    let c = one(&env, "in file.txt: true");
    assert!(
        c[0].contains("FAILED (not run)") && c[0].contains("not a folder"),
        "{c:?}"
    );
}

#[test]
fn refused_folder_goes_back_to_the_model() {
    let env = TestEnv::new();
    let (_, engine) = run(
        &env,
        json!(["in nope: true"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("cannot"),
        ],
    );
    let reqs = engine.requests();
    let last_user = reqs[2]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == oxa_proto::Role::User)
        .unwrap();
    assert!(last_user.content.as_ref().unwrap().contains("does not exist"));
}

#[test]
fn done_failed_flag_tracks_the_last_check_round() {
    let env = TestEnv::new();
    let (_, _, failed) = run_state(
        &env,
        json!(["exit 1"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            write("c2", "b.txt"),
            ScriptTurn::text("done"),
            write("c3", "c.txt"),
            ScriptTurn::text("done"),
        ],
        Arc::new(Direct),
    );
    assert!(failed, "two repair rounds used up and the check still fails");

    // Fixed within the rounds: not failed.
    let env = TestEnv::new();
    let (_, _, failed) = run_state(
        &env,
        json!([CHECK]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            write("c2", "fixed.txt"),
            ScriptTurn::text("done"),
        ],
        Arc::new(Direct),
    );
    assert!(!failed);

    // The model stops without fixing it (no new edit, so no re-run): still failed at the end.
    let env = TestEnv::new();
    let (_, _, failed) = run_state(
        &env,
        json!(["exit 1"]),
        vec![
            write("c1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::text("cannot fix"),
        ],
        Arc::new(Direct),
    );
    assert!(failed);

    // No edits, no checks, no flag.
    let env = TestEnv::new();
    let (_, _, failed) = run_state(
        &env,
        json!(["exit 1"]),
        vec![ScriptTurn::text("hi")],
        Arc::new(Direct),
    );
    assert!(!failed);
}

#[test]
fn in_dir_recording_sandbox_cwd_stays_at_work_folder() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("sub/deep")).unwrap();
    std::fs::write(env.work().join("sub/marker.txt"), "x").unwrap();
    let recording = Arc::new(Recording {
        specs: std::sync::Mutex::new(Vec::new()),
    });
    let (rec, _) = run_with(
        &env,
        json!(["in sub: test -f marker.txt"]),
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
        recording.clone(),
    );
    assert_eq!(
        rec.checks(),
        vec!["done check: [sub] test -f marker.txt ok".to_string()]
    );
    let specs = recording.specs.lock().unwrap();
    assert_eq!(specs.len(), 1);
    let s = &specs[0];
    // The sandbox authority stays at the original work folder, never the child.
    assert_eq!(s.cwd, env.work().canonicalize().unwrap());
    assert!(s.writable);
    // The command goes through the workspace-relative cd, not an absolute child path.
    assert!(
        s.command.starts_with("cd -- 'sub' || exit 111\n"),
        "command: {}",
        s.command
    );
}

#[test]
fn child_local_shell_prefix_runs_in_the_child_folder() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("sub")).unwrap();
    // Exists only in the child: the prefix fails unless it starts there.
    std::fs::write(env.work().join("sub/activate.sh"), "touch prefix-ran\n").unwrap();
    let settings = Settings::from_values(
        json!({
            "done": ["in sub: test -f prefix-ran"],
            "shellCommandPrefix": ". ./activate.sh",
        }),
        json!({}),
    );
    let (rec, _, failed) = run_settings(
        &env,
        settings,
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
        Arc::new(Direct),
    );
    assert_eq!(
        rec.checks(),
        vec!["done check: [sub] test -f prefix-ran ok".to_string()]
    );
    assert!(!failed);
    assert!(env.work().join("sub/prefix-ran").exists());
}

#[test]
fn swapped_subdir_symlink_cannot_move_the_bind_root() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.work().join("sub")).unwrap();
    std::fs::create_dir_all(env.root().join("outside")).unwrap();
    let recording = Arc::new(SwapChild {
        specs: std::sync::Mutex::new(Vec::new()),
        work: env.work(),
        outside: env.root().join("outside"),
    });
    // After validation and just before the command runs, `run` replaces the child with a
    // symlink that leaves the workspace. Validation was on the real folder, so it is accepted.
    // The command's exit code is not deterministic under Direct (the `cd` may land outside
    // through the swapped symlink), so we assert only cwd and the command text.
    let (_rec, _) = run_with(
        &env,
        json!(["in sub: touch ran.txt"]),
        vec![write("c1", "a.txt"), ScriptTurn::text("done")],
        recording.clone(),
    );
    let specs = recording.specs.lock().unwrap();
    assert_eq!(specs.len(), 1);
    let s = &specs[0];
    assert_eq!(s.cwd, env.work().canonicalize().unwrap());
    assert!(s.writable);
    assert!(
        s.command.starts_with("cd -- 'sub' || exit 111\n"),
        "command: {}",
        s.command
    );
}

#[test]
fn bash_made_change_in_a_git_repo_triggers_checks() {
    let env = TestEnv::new();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(env.work())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    let (rec, _) = run(
        &env,
        json!(["true"]),
        vec![
            ScriptTurn::tool(
                "b1",
                "bash",
                &json!({"command": "echo hi > made-by-bash.txt"}).to_string(),
            ),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(rec.checks(), vec!["done check: true ok".to_string()]);
}

#[test]
fn bash_without_changes_runs_no_checks() {
    let env = TestEnv::new();
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(env.work())
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let (rec, _) = run(
        &env,
        json!(["true"]),
        vec![
            ScriptTurn::tool("b1", "bash", &json!({"command": "ls"}).to_string()),
            ScriptTurn::text("done"),
        ],
    );
    assert!(rec.checks().is_empty());
}

fn git_in(env: &TestEnv, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(env.work())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?}");
}

#[test]
fn bash_edit_of_an_already_untracked_file_triggers_checks() {
    let env = TestEnv::new();
    git_in(&env, &["init", "-q"]);
    std::fs::write(env.work().join("new.rs"), "a\n").unwrap();
    let (rec, _) = run(
        &env,
        json!(["true"]),
        vec![
            ScriptTurn::tool("b1", "bash", &json!({"command": "echo b >> new.rs"}).to_string()),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(rec.checks(), vec!["done check: true ok".to_string()]);
}

#[test]
fn fingerprint_does_not_run_repository_diff_helpers() {
    let env = TestEnv::new();
    git_in(&env, &["init", "-q"]);
    git_in(&env, &["config", "user.email", "t@example.invalid"]);
    git_in(&env, &["config", "user.name", "t"]);
    std::fs::write(env.work().join("t.txt"), "a\n").unwrap();
    git_in(&env, &["add", "t.txt"]);
    git_in(&env, &["commit", "-q", "-m", "x"]);
    std::fs::write(env.work().join("helper.sh"), "#!/bin/sh\ntouch helper-ran\n").unwrap();
    let helper = env.work().join("helper.sh");
    git_in(&env, &["config", "diff.external", helper.to_str().unwrap()]);
    git_in(&env, &["config", "core.fsmonitor", "touch fsmonitor-ran; false"]);
    std::fs::write(env.work().join(".gitattributes"), "*.txt diff=x\n").unwrap();
    git_in(&env, &["config", "diff.x.textconv", "touch textconv-ran; cat"]);
    std::fs::set_permissions(&helper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::write(env.work().join("t.txt"), "b\n").unwrap();
    let _ = run(
        &env,
        json!(["true"]),
        vec![
            ScriptTurn::tool("b1", "bash", &json!({"command": "ls"}).to_string()),
            ScriptTurn::text("done"),
        ],
    );
    for m in ["helper-ran", "fsmonitor-ran", "textconv-ran"] {
        assert!(!env.work().join(m).exists(), "{m} executed on the host");
    }
}

#[test]
fn fingerprint_does_not_run_repository_filters() {
    let env = TestEnv::new();
    git_in(&env, &["init", "-q"]);
    git_in(&env, &["config", "user.email", "t@example.invalid"]);
    git_in(&env, &["config", "user.name", "t"]);
    std::fs::write(env.work().join(".gitattributes"), "*.txt filter=x\n").unwrap();
    std::fs::write(env.work().join("t.txt"), "a\n").unwrap();
    git_in(&env, &["add", "."]);
    git_in(&env, &["commit", "-q", "-m", "x"]);
    git_in(&env, &["config", "filter.x.clean", "touch filter-ran; cat"]);
    let _ = std::fs::remove_file(env.work().join("filter-ran"));
    std::fs::write(env.work().join("t.txt"), "bb\n").unwrap();
    let _ = run(
        &env,
        json!(["true"]),
        vec![
            ScriptTurn::tool("b1", "bash", &json!({"command": "echo c >> t.txt"}).to_string()),
            ScriptTurn::text("done"),
        ],
    );
    assert!(
        !env.work().join("filter-ran").exists(),
        "filter executed on the host"
    );
}

#[test]
fn tree_change_made_by_a_check_is_not_blamed_on_the_model() {
    let env = TestEnv::new();
    git_in(&env, &["init", "-q"]);
    let (rec, _) = run(
        &env,
        json!(["echo x >> gen.txt; false"]),
        vec![
            write("w1", "a.txt"),
            ScriptTurn::text("done"),
            ScriptTurn::tool("b1", "bash", &json!({"command": "ls"}).to_string()),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(rec.checks().len(), 1, "{:?}", rec.checks());
}

/// A run the repeat guard stopped is a failed run (exit 1, class `repeat_guard`), and the done
/// checks still run over the files it edited before it got stuck.
#[test]
fn repeat_guard_stop_is_a_failure_and_still_runs_the_done_checks() {
    let env = TestEnv::new();
    std::fs::write(env.work().join("b.txt"), "x").unwrap();
    let mut turns = vec![write("w1", "a.txt")];
    turns.extend((0..8).map(|i| ScriptTurn::tool(&format!("r{i}"), "read", r#"{"path":"b.txt"}"#)));
    let engine = Arc::new(ScriptedEngine::new(turns));
    let settings = Settings::from_values(json!({ "done": ["echo CHECKED; exit 1"] }), json!({}));
    let deps = Deps {
        engine: engine.clone(),
        store: Arc::new(MemStore::new()),
        tools: oxa_tools::builtin_tools(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(Direct),
        settings,
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    let mut s = Session::open(deps, Open::New).expect("open");
    let mut rec = Rec::default();
    s.prompt("go".into(), Vec::new(), &mut rec, &mut HeadlessUi::default())
        .expect("prompt");
    assert_eq!(*s.outcome(), oxa_core::RunOutcome::Failed("repeat_guard"));
    assert_eq!(s.outcome().exit_code(), 1);
    let checks = rec.checks();
    assert_eq!(checks.len(), 1, "{checks:?}");
    assert!(checks[0].contains("CHECKED"), "{checks:?}");
    // The failure is reported; nothing is sent back to the model after the stop.
    assert_eq!(engine.requests().len(), 7);
}

/// A hook set that offers tools (how extension, MCP and sub-agent tools reach a session).
struct ExtTools(Vec<Arc<dyn Tool>>);

impl Hooks for ExtTools {
    fn fire(&self, _p: HookPoint, _payload: &serde_json::Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.0.clone()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
}

/// An extension / MCP / sub-agent style tool: not a built-in, writes a file when `write` is set.
struct Outside {
    spec: ToolSpec,
    write: bool,
}

impl Outside {
    fn tool(name: &str, write: bool) -> Arc<dyn Tool> {
        Arc::new(Outside {
            spec: ToolSpec {
                name: name.to_string(),
                description: "test tool".to_string(),
                schema_json: "{}".to_string(),
                class: ToolClass::Read,
                exposure: Exposure::Direct,
                source: Source::Extension("ext.js".to_string()),
            },
            write,
        })
    }
}

impl Tool for Outside {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn default_selected(&self) -> bool {
        true
    }
    fn access(&self, _args: &serde_json::Value, _cx: &ToolCtx) -> Result<Vec<Access>, String> {
        Ok(Vec::new())
    }
    fn run(&self, _args: &serde_json::Value, cx: &ToolCtx, _live: &mut dyn FnMut(&str)) -> ToolOutput {
        if self.write {
            std::fs::write(cx.cwd.join("made-by-tool.txt"), "x").unwrap();
        }
        ToolOutput::ok("ok")
    }
}

fn outside(env: &TestEnv, write: bool) -> Vec<String> {
    let settings = Settings::from_values(json!({ "done": ["true"] }), json!({}));
    let (rec, _, _) = run_tools(
        env,
        settings,
        vec![ScriptTurn::tool("t1", "mcp_edit", "{}"), ScriptTurn::text("done")],
        Arc::new(Direct),
        vec![Outside::tool("mcp_edit", write)],
    );
    rec.checks()
}

#[test]
fn non_builtin_tool_edit_in_a_git_repo_triggers_checks() {
    let env = TestEnv::new();
    git_in(&env, &["init", "-q"]);
    assert_eq!(outside(&env, true), vec!["done check: true ok".to_string()]);
}

#[test]
fn non_builtin_tool_edit_outside_git_triggers_checks() {
    let env = TestEnv::new();
    assert_eq!(outside(&env, true), vec!["done check: true ok".to_string()]);
}

#[test]
fn non_builtin_tool_edit_outside_git_with_unreadable_subdirectory_triggers_checks() {
    use std::os::unix::fs::PermissionsExt;

    let env = TestEnv::new();
    let blocked = env.work().join("unreadable");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(std::fs::read_dir(&blocked).is_err(), "fixture must be unreadable");
    let unchanged = outside(&env, false);
    let changed = outside(&env, true);
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(unchanged.is_empty(), "{unchanged:?}");
    assert_eq!(changed, vec!["done check: true ok".to_string()]);
}

#[test]
fn non_builtin_tool_without_changes_runs_no_checks() {
    let env = TestEnv::new();
    std::fs::write(env.work().join("a.txt"), "a").unwrap();
    assert!(outside(&env, false).is_empty());
    git_in(&env, &["init", "-q"]);
    assert!(outside(&env, false).is_empty());
}

#[test]
fn bash_made_change_outside_git_triggers_checks() {
    let env = TestEnv::new();
    let (rec, _) = run(
        &env,
        json!(["true"]),
        vec![
            ScriptTurn::tool(
                "b1",
                "bash",
                &json!({"command": "echo hi > made-by-bash.txt"}).to_string(),
            ),
            ScriptTurn::text("done"),
        ],
    );
    assert_eq!(rec.checks(), vec!["done check: true ok".to_string()]);
}
