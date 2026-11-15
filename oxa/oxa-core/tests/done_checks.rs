//! Issue #104: the `done` setting. oxa runs the project's done commands itself when a run
//! that edited files is about to finish. Scripted engine, isolated `TestEnv`, real sandbox-less
//! shell in the work folder.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{AgentEvent, CancelToken, EventSink, ExecResult, ExecSpec, HeadlessUi, OxaError, Sandbox};
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
    let engine = Arc::new(ScriptedEngine::new(turns));
    let settings = if done.is_null() {
        Settings::default()
    } else {
        Settings::from_values(json!({ "done": done }), json!({}))
    };
    let deps = Deps {
        engine: engine.clone(),
        store: Arc::new(MemStore::new()),
        tools: oxa_tools::builtin_tools(),
        hooks: Vec::new(),
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
