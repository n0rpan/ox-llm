//! The director in the loop (animations design 3.2 and 3.3, slice 4): zero work when idle, one
//! still frame while waiting for the user, slow and settled scenes step once a second, and a
//! pinned scene bypasses the picks. Public API only; the clock is passed in.
//!
//! Desk Z06, at the end of the file: the session worker without its periodic wake
//! (`WorkerStop::without_idle_tick`). These tests run a real worker thread on a real clock.
use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_interactive::skin::stage::director::{Director, Pending};
use oxa_interactive::skin::stage::load_pack;
use oxa_interactive::{
    worker_loop_with, ReopenFn, SessionCmd, SessionInfo, UiRequest, WorkerMsg, WorkerStop,
};
use oxa_proto::{
    AgentEvent, CommandSpec, Delivery, HookPoint, HookReply, Hooks, Host, HostAction, OxaError, Tool, Ui,
};
use oxa_resources::Resources;
use oxa_session::MemStore;
use serde_json::{json, Value};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn frame(text: &str, inks: &str) -> Value {
    json!([text, inks])
}

/// A run scene on `on`, two frames at 8 and 12 cells, looping every `step` ms.
fn scene(id: &str, on: &str, looping: &str, step: u32) -> Value {
    json!({
        "id": id,
        "title": "a run",
        "on": [on],
        "tier": "run",
        "loop": looping,
        "step_ms": step,
        "significance": 0,
        "sizes": {
            "8": [frame("(ox>    ", "HHHH...."), frame("ox>     ", "HHH.....")],
            "12": [frame("(ox>        ", "HHHH........"), frame("ox>         ", "HHH.........")]
        },
        "captions": [["a caption", "a longer caption"]]
    })
}

fn director(scenes: Vec<Value>) -> Director {
    let text = json!({ "version": 1, "scenes": scenes }).to_string();
    let loaded = load_pack(&text).expect("the test pack is a version 1 pack");
    assert!(
        loaded.skipped.is_empty(),
        "the test scenes must all load: {:?}",
        loaded.skipped
    );
    Director::new("session-1", loaded.scenes)
}

fn working(key: &str) -> Pending {
    Pending::Working {
        key: key.to_string(),
        icon: '*',
    }
}

#[test]
fn no_idle_cpu() {
    let mut d = director(vec![
        scene("think.duck", "think", "loop", 125),
        scene("done.flag", "done", "once", 125),
    ]);
    d.update(Pending::Idle, 8, 0);
    assert_eq!(d.next_deadline_ms(0), None);
    let mut redraws = 0;
    for t in 0..10_000u64 {
        if d.redraw_due(t) {
            redraws += 1;
        }
    }
    assert_eq!(redraws, 0, "an idle stage never asks to be drawn");

    // A working scene does have a deadline, so the loop can sleep until it.
    d.update(working("think"), 8, 10_000);
    assert_eq!(d.next_deadline_ms(10_000), Some(10_125));

    // A finished celebration (a `once` scene after its last frame) has no deadline either.
    d.update(working("done"), 8, 20_000);
    assert_eq!(d.shown(20_300), None);
    assert_eq!(d.next_deadline_ms(20_300), None);
}

#[test]
fn waiting_for_the_user_is_one_still_frame() {
    let mut d = director(vec![scene("think.duck", "think", "loop", 125)]);
    d.update(working("think"), 8, 0);
    assert!(d.redraw_due(0), "the first frame is drawn");
    d.update(Pending::Waiting, 8, 1_000);
    assert_eq!(d.next_deadline_ms(1_000), None, "waiting has no timer");
    assert!(d.redraw_due(1_000), "the scene is taken away once");
    let mut redraws = 0;
    for t in (1_000..20_000u64).step_by(50) {
        if d.redraw_due(t) {
            redraws += 1;
        }
    }
    assert_eq!(redraws, 0, "the poster does not move while the user is asked");
    assert_eq!(d.shown(15_000), None);
}

#[test]
fn a_slow_scene_steps_once_a_second() {
    let mut d = director(vec![scene("wait.hourglass", "wait", "loop", 125)]);
    d.update(working("wait:ci"), 8, 0);
    assert_eq!(d.shown(0), Some(("wait.hourglass", 0)));
    // The scene's own step is 125 ms; a wait key holds each frame for a second.
    assert_eq!(d.shown(124), Some(("wait.hourglass", 0)));
    assert_eq!(d.shown(999), Some(("wait.hourglass", 0)));
    assert_eq!(d.shown(1_000), Some(("wait.hourglass", 1)));
    assert_eq!(d.next_deadline_ms(0), Some(1_000));
}

#[test]
fn a_scene_settles_after_twenty_seconds() {
    let mut d = director(vec![scene("think.duck", "think", "loop", 125)]);
    d.update(working("think"), 8, 0);
    // Before the settle time the scene keeps its own 125 ms step.
    assert_eq!(d.next_deadline_ms(19_000), Some(19_125));
    // After twenty seconds of the same activity it steps once a second.
    assert_eq!(d.next_deadline_ms(20_000), Some(21_000));
    // A new run restarts the settle clock: idle, then the same key again.
    d.update(Pending::Idle, 8, 20_000);
    d.update(working("think"), 8, 20_000);
    assert_eq!(d.next_deadline_ms(20_000), Some(20_125));
}

#[test]
fn a_pinned_scene_bypasses_the_director() {
    let mut d = director(vec![
        scene("think.duck", "think", "loop", 125),
        scene("tool.bash", "tool:bash", "loop", 125),
    ]);
    d.pin(Some("tool.bash"));
    d.update(working("think"), 8, 0);
    assert_eq!(
        d.shown(0),
        Some(("tool.bash", 0)),
        "the pin plays whatever the activity is"
    );
    d.pin(None);
    d.update(Pending::Idle, 8, 1_000);
    d.update(working("think"), 8, 1_000);
    assert_eq!(
        d.shown(1_000),
        Some(("think.duck", 0)),
        "releasing the pin returns to the picks"
    );
}

// ---- Desk Z06: the session worker without a periodic wake ----

struct AllowAll;

impl oxa_proto::Policy for AllowAll {
    fn decide(
        &self,
        _mode: oxa_proto::Mode,
        _a: &oxa_proto::Access,
        _cwd: &std::path::Path,
    ) -> oxa_proto::Decision {
        oxa_proto::Decision::Allow
    }
    fn remember(&mut self, _a: &oxa_proto::Access) {}
}

struct NoBash;

impl oxa_proto::Sandbox for NoBash {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &oxa_proto::ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(
        &self,
        _s: &oxa_proto::ExecSpec,
        _c: &oxa_proto::CancelToken,
        _live: &mut dyn FnMut(&[u8]),
    ) -> oxa_proto::ExecResult {
        oxa_proto::ExecResult::default()
    }
}

/// A hook set that only keeps its host, so the test can post a host request like an extension.
#[derive(Default)]
struct HostKeeper {
    host: Mutex<Option<Arc<dyn Host>>>,
}

impl Hooks for HostKeeper {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Err(OxaError::new(oxa_proto::codes::NOT_FOUND, name))
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn attach(&self, host: Arc<dyn Host>) {
        *self.host.lock().unwrap() = Some(host);
    }
}

struct Worker {
    cmds: Arc<Sender<SessionCmd>>,
    msgs: Receiver<WorkerMsg>,
    _ui: Receiver<UiRequest>,
    done: Receiver<Option<SessionInfo>>,
    stop: WorkerStop,
    host: Arc<dyn Host>,
    _env: TestEnv,
}

/// A worker thread over a fresh session with one scripted reply, announced and ready.
fn idle_worker(stop: WorkerStop) -> Worker {
    let env = TestEnv::new();
    let hooks = Arc::new(HostKeeper::default());
    let session = Session::open(
        Deps {
            engine: Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("pong")])),
            store: Arc::new(MemStore::new()),
            tools: Vec::new(),
            hooks: vec![hooks.clone()],
            policy: Box::new(AllowAll),
            sandbox: Arc::new(NoBash),
            settings: Settings::default(),
            dirs: Dirs::under(env.root()),
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources: Resources::default(),
            model: None,
            thinking: None,
            mode: None,
            session_id: None,
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .expect("open");
    let host = hooks.host.lock().unwrap().clone().expect("attached");
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (msg_tx, msg_rx) = mpsc::channel();
    let (ui_tx, ui_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let cmd_tx = Arc::new(cmd_tx);
    let wake = Arc::downgrade(&cmd_tx);
    let worker_stop = stop.clone();
    let reopen: ReopenFn = Box::new(|_, _| Err(OxaError::new("invalid", "no reopen in this test")));
    std::thread::spawn(move || {
        let last = worker_loop_with(session, reopen, cmd_rx, msg_tx, ui_tx, worker_stop, wake);
        let _ = done_tx.send(last);
    });
    assert!(
        matches!(
            msg_rx.recv_timeout(Duration::from_secs(10)),
            Ok(WorkerMsg::Session(_))
        ),
        "the worker announces its session before it waits"
    );
    Worker {
        cmds: cmd_tx,
        msgs: msg_rx,
        _ui: ui_rx,
        done: done_rx,
        stop,
        host,
        _env: env,
    }
}

/// What an extension does from a background thread: a message that starts a turn.
fn trigger(text: &str) -> HostAction {
    HostAction::SendMessage {
        custom_type: "test".into(),
        content: text.into(),
        display: true,
        details: Value::Null,
        deliver: Delivery::FollowUp,
        trigger_turn: true,
    }
}

/// Whether a triggered turn started and settled within ten seconds.
fn turn_ran(w: &Worker) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    let mut started = false;
    while Instant::now() < end {
        match w.msgs.recv_timeout(Duration::from_millis(50)) {
            Ok(WorkerMsg::Event(AgentEvent::AgentStart)) => started = true,
            Ok(WorkerMsg::Event(AgentEvent::AgentSettled { .. })) => return started,
            _ => {}
        }
    }
    false
}

/// The desktop's stop condition "no steady wakeups": a worker asked to block never leaves its
/// wait while nothing happens. A host request posted while it blocks is still served, through
/// the session's waker, and the stop signal reaches it too.
#[test]
fn a_worker_without_the_idle_tick_makes_no_wakeups_and_still_serves_the_host() {
    let w = idle_worker(WorkerStop::without_idle_tick());
    assert!(w.stop.blocks_when_idle());
    std::thread::sleep(Duration::from_secs(10));
    assert_eq!(w.stop.idle_wakes(), 0, "an idle session thread makes no wakeups");

    w.host.call(trigger("ping")).expect("accepted");
    assert!(turn_ran(&w), "the host request is served with no tick");
    assert!(w.stop.idle_wakes() >= 1, "it was served through the waker");

    w.stop.stop();
    w.done
        .recv_timeout(Duration::from_secs(10))
        .expect("the stop signal ends a worker that blocks");
}

/// The terminal's worker is unchanged: it keeps its tick.
#[test]
fn the_default_worker_keeps_its_idle_tick() {
    let w = idle_worker(WorkerStop::new());
    assert!(!w.stop.blocks_when_idle());
    let end = Instant::now() + Duration::from_secs(10);
    while w.stop.idle_wakes() < 2 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(w.stop.idle_wakes() >= 2, "the default worker ticks while idle");
    let _ = w.cmds.send(SessionCmd::Shutdown);
    w.done
        .recv_timeout(Duration::from_secs(10))
        .expect("worker ended");
}

/// A worker that blocks holds no sender of its own: dropping the front end's ends it.
#[test]
fn dropping_the_senders_ends_a_worker_that_blocks() {
    let w = idle_worker(WorkerStop::without_idle_tick());
    drop(w.cmds);
    w.done
        .recv_timeout(Duration::from_secs(10))
        .expect("the worker ended when its command channel closed");
}
