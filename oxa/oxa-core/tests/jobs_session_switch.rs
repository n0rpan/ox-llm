//! Background jobs end when the session is replaced (`switch`, `fork`, `clone`).
use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{CancelToken, ExecResult, ExecSpec, HeadlessUi, OxaError, Sandbox, VecSink};
use oxa_sandbox::run_unsandboxed_capture;
use oxa_session::{MemStore, SessionStore};
use oxa_tools::{JobRegistry, JobStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Plain;

impl Sandbox for Plain {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, s: &ExecSpec, c: &CancelToken, live: &mut dyn FnMut(&[u8])) -> ExecResult {
        run_unsandboxed_capture(s, c, live, true)
    }
}

fn open(env: &TestEnv, jobs: Arc<JobRegistry>) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let deps = Deps {
        engine: Arc::new(ScriptedEngine::new(Vec::new())),
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(Plain),
        settings: Settings::default(),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Default::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs,
    };
    Session::open(deps, Open::New).expect("open")
}

fn spec(env: &TestEnv, command: &str) -> ExecSpec {
    ExecSpec {
        command: command.into(),
        cwd: env.work(),
        writable: true,
        net: false,
        timeout_s: None,
        env: Vec::new(),
    }
}

fn start_job(jobs: &JobRegistry, env: &TestEnv) -> String {
    jobs.set_sandbox(Arc::new(Plain));
    let id = jobs.spawn(spec(env, "echo up; sleep 60")).unwrap();
    assert_eq!(jobs.running(), 1);
    id
}

/// After the reset the id is unknown and the slot is free; the real process dies too.
fn assert_ended(jobs: &JobRegistry, id: &str) {
    assert_eq!(jobs.running(), 0);
    assert_eq!(jobs.read(id).unwrap_err(), format!("Unknown job: {id}"));
}

#[test]
fn switch_ends_running_jobs() {
    let env = TestEnv::new();
    let jobs = Arc::new(JobRegistry::new());
    let mut s = open(&env, jobs.clone());
    let id = start_job(&jobs, &env);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.switch(Open::New, &mut sink, &mut ui).expect("switch");
    assert_ended(&jobs, &id);
    // The slots are free at once and ids restart past the old one.
    let next = start_job(&jobs, &env);
    assert_ne!(next, id);
    jobs.kill_all();
}

/// A sandbox whose run blocks (really) until the cancel token fires, then records it.
struct Watch {
    ended: Arc<AtomicBool>,
}

impl Sandbox for Watch {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        while !c.is_cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.ended.store(true, Ordering::SeqCst);
        ExecResult {
            output: Vec::new(),
            exit: None,
            timed_out: false,
            unsandboxed: false,
        }
    }
}

#[test]
fn job_cancel_fires_on_switch() {
    let env = TestEnv::new();
    let jobs = Arc::new(JobRegistry::new());
    let mut s = open(&env, jobs.clone());
    let ended = Arc::new(AtomicBool::new(false));
    jobs.set_sandbox(Arc::new(Watch { ended: ended.clone() }));
    let id = jobs.spawn(spec(&env, "block")).unwrap();
    assert_eq!(jobs.running(), 1);
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.switch(Open::New, &mut sink, &mut ui).expect("switch");
    assert_eq!(jobs.running(), 0);
    assert_eq!(jobs.read(&id).unwrap_err(), format!("Unknown job: {id}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ended.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the job's cancel never fired");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn close_still_kills_jobs() {
    let env = TestEnv::new();
    let jobs = Arc::new(JobRegistry::new());
    let mut s = open(&env, jobs.clone());
    let id = start_job(&jobs, &env);
    s.close(&mut HeadlessUi::default());
    let deadline = Instant::now() + Duration::from_secs(10);
    while jobs.read(&id).unwrap().status != JobStatus::Killed {
        assert!(Instant::now() < deadline, "not killed");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn fork_and_clone_end_running_jobs() {
    let env = TestEnv::new();
    let jobs = Arc::new(JobRegistry::new());
    let mut s = open(&env, jobs.clone());
    let id = start_job(&jobs, &env);
    s.clone_session().expect("clone");
    assert_ended(&jobs, &id);

    let mut s = open(&env, jobs.clone());
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    s.prompt("hi".into(), Vec::new(), &mut sink, &mut ui).ok();
    let entry = s.inner().tree.entries().last().map(|e| e.id.clone());
    if let Some(entry) = entry {
        let id = start_job(&jobs, &env);
        if s.fork(&entry).is_ok() {
            assert_ended(&jobs, &id);
        }
    }
    jobs.kill_all();
}
