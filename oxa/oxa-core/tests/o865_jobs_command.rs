//! #865: the `/jobs` built-in — list the session's background jobs, stop one by id.
//! Isolation: every file lives under `oxa_fake::TestEnv`; the sandbox runs a real `sh` in the
//! test's own work directory.
use oxa_config::{Dirs, Settings};
use oxa_core::{run_builtin, Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{codes, CancelToken, ExecResult, ExecSpec, HeadlessUi, OxaError, Sandbox, VecSink};
use oxa_sandbox::run_unsandboxed_capture;
use oxa_session::MemStore;
use oxa_tools::JobStatus;
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

fn open(env: &TestEnv, jobs: Arc<oxa_core::JobRegistry>) -> Session {
    let deps = Deps {
        engine: Arc::new(ScriptedEngine::new(Vec::new())),
        store: Arc::new(MemStore::new()),
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

fn run(s: &mut Session, args: &str) -> Result<Vec<String>, OxaError> {
    run_builtin(
        s,
        "jobs",
        args,
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
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

/// Start a job that prints one line and then blocks, and wait until that line is buffered.
fn start(env: &TestEnv, jobs: &oxa_core::JobRegistry) -> String {
    jobs.set_sandbox(Arc::new(Plain));
    let id = jobs.spawn(spec(env, "echo hello; sleep 60")).expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(10);
    while jobs.buffered_len(&id).unwrap_or(0) == 0 {
        assert!(Instant::now() < deadline, "the job never wrote its line");
        std::thread::sleep(Duration::from_millis(5));
    }
    id
}

#[test]
fn empty_listing_says_so() {
    let env = TestEnv::new();
    let jobs = Arc::new(oxa_core::JobRegistry::new());
    let mut s = open(&env, jobs);
    assert_eq!(run(&mut s, "").expect("jobs"), vec!["No background jobs."]);
    // Whitespace-only arguments are the same as none.
    assert_eq!(run(&mut s, "  ").expect("jobs"), vec!["No background jobs."]);
}

#[test]
fn listing_shows_id_status_age_and_last_line() {
    let env = TestEnv::new();
    let jobs = Arc::new(oxa_core::JobRegistry::new());
    let mut s = open(&env, jobs.clone());
    let id = start(&env, &jobs);
    let lines = run(&mut s, "").expect("jobs");
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "Background jobs (1):");
    assert!(lines[1].starts_with(&format!("  {id}")), "{lines:?}");
    assert!(lines[1].contains("running"), "{lines:?}");
    assert!(lines[1].ends_with("hello"), "{lines:?}");
    // The listing is a snapshot: the output a `bash {job}` read would show is still unseen.
    assert_eq!(jobs.unseen_len(&id), Some(6));
    assert_eq!(jobs.read(&id).expect("read").output, "hello\n");
    jobs.kill_all();
}

#[test]
fn stop_ends_one_job_and_leaves_the_others() {
    let env = TestEnv::new();
    let jobs = Arc::new(oxa_core::JobRegistry::new());
    let mut s = open(&env, jobs.clone());
    jobs.set_sandbox(Arc::new(Plain));
    // Two real jobs. The first writes a file after a second, so a job that kept running after
    // the stop leaves its file behind. The second waits for a file the test writes, so it is
    // still running when the stop returns whatever the machine's load (a fixed sleep raced the
    // stop on a loaded runner).
    let first = jobs
        .spawn(spec(&env, "echo hello; sleep 1; echo late > late.txt"))
        .expect("spawn");
    let second = jobs
        .spawn(spec(
            &env,
            "echo second; while [ ! -f go.txt ]; do sleep 0.1; done; echo done > done.txt",
        ))
        .expect("spawn");
    assert_eq!(jobs.running(), 2);
    let lines = run(&mut s, &format!("stop {first}")).expect("stop");
    assert_eq!(lines, vec![format!("Stopped {first} (killed).")]);
    assert_eq!(jobs.running(), 1);
    assert_eq!(jobs.read(&first).expect("read").status, JobStatus::Killed);
    assert_eq!(jobs.read(&second).expect("read").status, JobStatus::Running);
    // The stopped job's process group is gone: its second command never ran.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !env.work().join("late.txt").exists(),
        "the stopped job kept running"
    );
    // The other job is untouched: it runs to completion once the test lets it.
    std::fs::write(env.work().join("go.txt"), b"go").expect("go");
    let deadline = Instant::now() + Duration::from_secs(10);
    while jobs.read(&second).expect("read").status == JobStatus::Running {
        assert!(Instant::now() < deadline, "the second job never finished");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(jobs.read(&second).expect("read").status, JobStatus::Exited(0));
    assert!(env.work().join("done.txt").exists(), "the other job completed");
    // The listing shows the killed job first, oldest first.
    let lines = run(&mut s, "").expect("jobs");
    assert_eq!(lines[0], "Background jobs (2):");
    assert!(lines[1].contains("killed"), "{lines:?}");
    assert!(lines[2].contains("exited 0"), "{lines:?}");
    jobs.kill_all();
}

#[test]
fn stop_reports_a_job_that_had_already_finished() {
    let env = TestEnv::new();
    let jobs = Arc::new(oxa_core::JobRegistry::new());
    let mut s = open(&env, jobs.clone());
    jobs.set_sandbox(Arc::new(Plain));
    let id = jobs.spawn(spec(&env, "true")).expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(10);
    while jobs.read(&id).expect("read").status == JobStatus::Running {
        assert!(Instant::now() < deadline, "the job never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
    let lines = run(&mut s, &format!("stop {id}")).expect("stop");
    assert_eq!(lines, vec![format!("{id} had already finished (exited 0).")]);
}

#[test]
fn bad_arguments_are_usage_errors() {
    let env = TestEnv::new();
    let jobs = Arc::new(oxa_core::JobRegistry::new());
    let mut s = open(&env, jobs.clone());
    for args in ["stop", "stop job-1 extra", "nonsense"] {
        let e = run(&mut s, args).expect_err(args);
        assert_eq!(e.code, codes::USAGE, "{args}: {e:?}");
        assert_eq!(e.message, "Usage: /jobs [stop <id>]", "{args}");
    }
    // An unknown id is the registry's own error, and it never touches a running job.
    let id = start(&env, &jobs);
    let e = run(&mut s, "stop job-99").expect_err("unknown");
    assert_eq!(e.code, codes::INVALID);
    assert_eq!(e.message, "Unknown job: job-99");
    assert_eq!(jobs.running(), 1);
    assert_eq!(jobs.read(&id).expect("read").status, JobStatus::Running);
    jobs.kill_all();
}
