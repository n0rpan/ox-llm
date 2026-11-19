//! The tool queue of the activity line (TOOL-QUEUE.md), on the core's own event sequence: a
//! `Session` on the scripted engine with the real `read` tool. A batch (several calls in one
//! reply) shows its size and every pending call before the first result; a sequence (one call
//! per reply) counts what is behind it and names no total.
use oxa_config::{Dirs, Keymap, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_interactive::{
    apply_event, oxa_arcade_line, tool_batch, tool_queue_detail, transcript_lines, Activity, Animation,
    ArcadeRun, ArcadeScene, Block, Skin, StatusModel, ToolState, ViewModel, ViewSettings,
};
use oxa_proto::{
    Access, AgentEvent, CancelToken, Decision, Engine, ExecResult, ExecSpec, Finish, Mode, OxaError, Policy,
    Sandbox, Tool, ToolCall,
};
use oxa_resources::Resources;
use oxa_session::{MemStore, SessionStore};
use oxa_tui::{strip_ansi, Theme};
use std::path::Path;
use std::sync::Arc;

struct AllowAll;

impl Policy for AllowAll {
    fn decide(&self, _mode: Mode, _a: &Access, _cwd: &Path) -> Decision {
        Decision::Allow
    }
    fn remember(&mut self, _a: &Access) {}
}

struct NoBash;

impl Sandbox for NoBash {
    fn check(&self) -> Result<(), OxaError> {
        Ok(())
    }
    fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
        Vec::new()
    }
    fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
        ExecResult {
            output: Vec::new(),
            exit: Some(0),
            timed_out: false,
            unsandboxed: false,
        }
    }
}

const FILES: [&str; 4] = ["alpha.rs", "beta.rs", "gamma.rs", "delta.rs"];

fn read_call(i: usize, file: &str) -> ToolCall {
    ToolCall {
        id: format!("call_{i}"),
        name: "read".to_string(),
        args_raw: format!("{{\"path\":\"src/{file}\"}}"),
    }
}

/// The events of one run on the scripted engine; `missing` is not written to disk (its read
/// fails).
fn real_run(turns: Vec<ScriptTurn>, missing: Option<&str>) -> Vec<AgentEvent> {
    let env = TestEnv::new();
    for file in FILES.iter().filter(|f| Some(**f) != missing) {
        env.write(&format!("work/src/{file}"), "fn main() {}\n");
    }
    let deps = Deps {
        engine: Arc::new(ScriptedEngine::new(turns)) as Arc<dyn Engine>,
        store: Arc::new(MemStore::new()) as Arc<dyn SessionStore>,
        tools: vec![Arc::new(oxa_tools::ReadTool::new()) as Arc<dyn Tool>],
        hooks: Vec::new(),
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
    };
    let mut s = Session::open(deps, Open::Ephemeral).expect("open the test session");
    let mut sink = oxa_proto::VecSink(Vec::new());
    let mut ui = oxa_proto::HeadlessUi::default();
    s.prompt("read them".to_string(), Vec::new(), &mut sink, &mut ui)
        .expect("the run settles");
    sink.0
}

/// What the screen says after each event: the queue detail (plain) and the view model.
fn replay(events: &[AgentEvent]) -> Vec<(Option<String>, ViewModel, &AgentEvent)> {
    let mut vm = ViewModel::default();
    events
        .iter()
        .map(|e| {
            apply_event(&mut vm, e);
            let detail = tool_queue_detail(&vm.blocks, &vm.status.activity, Skin::Oxa);
            (detail, vm.clone(), e)
        })
        .collect()
}

fn oxa_settings() -> ViewSettings {
    ViewSettings {
        skin: Skin::Oxa,
        ..ViewSettings::default()
    }
}

#[test]
fn a_batch_of_four_shows_its_size_and_every_pending_call_before_the_first_result() {
    let batch = ScriptTurn {
        text: None,
        tool_calls: FILES.iter().enumerate().map(|(i, f)| read_call(i, f)).collect(),
        finish: Finish::ToolCalls,
        ..ScriptTurn::text("")
    };
    let events = real_run(vec![batch, ScriptTurn::text("done")], Some("gamma.rs"));
    let steps = replay(&events);
    let first_result = steps
        .iter()
        .position(|(_, _, e)| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
        .expect("a tool result");
    let first_start = steps
        .iter()
        .position(|(_, _, e)| matches!(e, AgentEvent::ToolExecutionStart { .. }))
        .expect("a tool start");

    // Before anything ran: the header with the total, and four pending calls on the line...
    let (detail, vm, _) = &steps[first_start - 1];
    assert_eq!(detail.as_deref(), Some("reading 4 files 0/4 ····"));
    let b = tool_batch(&vm.blocks).expect("the batch");
    assert_eq!((b.total(), b.finished()), (4, 0));
    assert!(
        b.states.iter().all(|s| *s == ToolState::Pending),
        "{:?}",
        b.states
    );
    // ...and in the transcript, each with its path.
    let transcript: Vec<String> = transcript_lines(vm, 80, &Theme::plain(), &oxa_settings())
        .0
        .iter()
        .map(|l| strip_ansi(l))
        .collect();
    for file in FILES {
        assert!(
            transcript.iter().any(|l| l.contains(&format!("src/{file}"))),
            "{file} is listed while pending:\n{}",
            transcript.join("\n")
        );
    }
    if std::env::var("OXA_DUMP").is_ok() {
        println!("{}", transcript.join("\n"));
    }
    let pending = transcript
        .iter()
        .filter(|l| l.starts_with('└') && l.contains("pending"));
    assert_eq!(pending.count(), 4, "{}", transcript.join("\n"));

    // The total was already on screen while the reply streamed: from the second call on.
    let early = steps[..first_start]
        .iter()
        .find_map(|(d, vm, _)| d.clone().map(|d| (d, vm.status.activity.clone())))
        .expect("a detail while streaming");
    assert_eq!(early, ("reading 2 files 0/2 ··".to_string(), Activity::Streaming));
    assert!(steps[..first_result]
        .iter()
        .any(|(d, _, _)| d.as_deref().is_some_and(|d| d.starts_with("reading 4 files 0/4"))));

    // Progress: every result moves the count; the missing file is a failure, not a hole.
    let progress: Vec<String> = steps
        .iter()
        .filter(|(_, _, e)| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
        .filter_map(|(d, _, _)| d.clone())
        .map(|d| d.split(' ').nth(3).unwrap_or_default().to_string())
        .collect();
    assert_eq!(progress, ["1/4", "2/4", "3/4", "4/4"]);
    let (last, vm, _) = steps
        .iter()
        .rfind(|(_, _, e)| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
        .expect("the last result");
    assert_eq!(last.as_deref(), Some("reading 4 files 4/4 ✓✓✗✓"));

    // While some of the batch still run, a tool runs (not "thinking").
    let running: Vec<&Activity> = steps
        .iter()
        .filter(|(_, _, e)| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
        .map(|(_, vm, _)| &vm.status.activity)
        .collect();
    assert_eq!(
        running.last(),
        Some(&&Activity::Thinking),
        "all done: the results go back"
    );
    for (_, vm, _) in &steps {
        let busy = vm
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Tool(t) if t.state == ToolState::Running));
        if busy {
            assert_eq!(vm.status.activity, Activity::Tool("read".to_string()));
        }
    }

    // On the activity line itself (the arcade's, in colour): one row, the detail after the badge.
    let theme = oxa_tui::builtin_theme("dark", oxa_tui::ColorMode::TrueColor, None).expect("dark");
    let st = StatusModel {
        working_message: tool_queue_detail(&vm.blocks, &vm.status.activity, Skin::Oxa),
        ..vm.status.clone()
    };
    let line = oxa_arcade_line(
        &st,
        &Keymap::defaults(),
        Animation::Arcade,
        ArcadeScene::Charge,
        &ArcadeRun {
            coins: 4,
            finish: None,
        },
        100,
        &theme,
        Skin::Oxa,
    )
    .expect("a line");
    assert!(
        strip_ansi(&line).contains(" THINKING   reading 4 files 4/4 ✓✓✗✓ · "),
        "{:?}",
        strip_ansi(&line)
    );
    assert!(!line.contains('\n'));

    // After the run: nothing left on the line.
    let (detail, vm, _) = steps.last().expect("events");
    assert_eq!((detail.as_deref(), &vm.status.activity), (None, &Activity::Idle));
}

#[test]
fn sequential_reads_count_what_is_done_and_never_name_a_total() {
    let turns: Vec<ScriptTurn> = FILES[..3]
        .iter()
        .enumerate()
        .map(|(i, f)| ScriptTurn::tool(&format!("call_{i}"), "read", &format!("{{\"path\":\"src/{f}\"}}")))
        .chain([ScriptTurn::text("done")])
        .collect();
    let events = real_run(turns, None);
    let steps = replay(&events);
    let at_start: Vec<String> = steps
        .iter()
        .filter(|(_, _, e)| matches!(e, AgentEvent::ToolExecutionStart { .. }))
        .map(|(d, _, _)| d.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        at_start,
        [
            "read alpha.rs",
            "read beta.rs · 1 file read so far",
            "read gamma.rs · 2 files read so far",
        ]
    );
    // No step of the run ever shows a total (`n/m`) or a batch header.
    for (detail, vm, _) in &steps {
        if let Some(d) = detail {
            assert!(!d.contains('/') && !d.contains("reading"), "a faked total: {d:?}");
        }
        if let Some(b) = tool_batch(&vm.blocks) {
            assert_eq!(b.total(), 1, "one call per reply");
        }
        assert!(vm.blocks.iter().filter(|b| matches!(b, Block::Tool(_))).count() <= 3);
    }
}
