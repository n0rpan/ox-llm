//! C1 regression tests: what survives compaction.
//! Compaction rewrites the transcript (the wire) only. The frozen prefix (system sections, hook
//! sections, the goal section), the queued follow-ups and the newest user prompt must still be
//! there afterwards. Isolation: `oxa_fake::TestEnv` for every file and directory.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::prompt::{goal_start, GOAL_CONTINUE};
use oxa_core::{append_entry, compact_now, open_epoch, Queue, SessionState, Shared, Stats, Toolset};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    CancelToken, CommandSpec, CompactReason, Engine, EntryBody, FrozenPrefix, HookPoint, HookReply, Hooks,
    Mode, OxaError, Policy, ThinkingLevel, Tool, Ui, Value, VecSink, WireMsg,
};
use oxa_resources::{ResourceSource, Resources, Skill};
use oxa_sandbox::NoSandbox;
use oxa_session::{Goal, GoalBudget, MemStore, SessionStore, Transcript, Tree};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------------------------
// Harness

/// Policy that keeps only the newest user message and what follows it (`keep_tokens: 1`), so
/// that a compaction cuts at the last prompt.
fn tight() -> CompactPolicy {
    CompactPolicy {
        soft: u32::MAX,
        hard: u32::MAX,
        keep_tokens: 1,
        summary_max: 1024,
        enabled: true,
    }
}

fn build(env: &TestEnv, engine: Arc<dyn Engine>, hooks: Vec<Arc<dyn Hooks>>) -> SessionState {
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let cwd = env.work();
    let sref = store.create(&cwd, None, None).expect("create session");
    let info = engine.info(None).expect("engine info");
    let sections = vec![
        ("preamble".to_string(), "You are a test agent.".to_string()),
        ("cwd".to_string(), cwd.display().to_string()),
    ];
    let mut prefix = FrozenPrefix {
        sections,
        tools_json: "[]".to_string(),
        sha256: [0u8; 32],
    };
    prefix.sha256 = Sha256::digest(prefix.hash_input()).into();
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue::default()),
        cancel: CancelToken::new(),
        cancel_retry: CancelToken::new(),
        busy: std::sync::atomic::AtomicBool::new(false),
        modes: Mutex::new((QueueMode::All, QueueMode::OneAtATime)),
        dirty: std::sync::atomic::AtomicBool::new(false),
        gate: Default::default(),
        wait_wake: CancelToken::new(),
    });
    SessionState {
        goal: None,
        output_schema: None,
        engine,
        store,
        hooks,
        policy: Box::new(ModePolicy::new()) as Box<dyn Policy>,
        sandbox: Arc::new(NoSandbox),
        settings: Settings::from_values(
            serde_json::json!({"compaction": {"reactivateSkills": true}}),
            Value::Object(Default::default()),
        ),
        dirs: Dirs::under(env.root()),
        cwd: cwd.clone(),
        all_tools: Vec::new(),
        selection: Default::default(),
        active: Toolset {
            direct: Vec::new(),
            indirect: Vec::new(),
        },
        resources: Resources::default(),
        sref,
        tree: Tree::new(Vec::new()),
        transcript: Transcript::default(),
        prefix,
        info,
        model: "fake-model".to_string(),
        thinking: ThinkingLevel::Off,
        mode: Mode::Workspace,
        compact: tight(),
        shared,
        next_turn: Vec::new(),
        last_usage: None,
        stats: Stats::default(),
        spill_dir: env.tmp(),
        tool_env: Vec::new(),
        tune: Default::default(),
        auto_compaction: true,
        auto_retry: true,
        host_rx: None,
        host_deferred: Vec::new(),
        compact_memo: Default::default(),
        jobs: oxa_core::fresh_jobs(),
    }
}

/// A session over a scripted engine with one summary reply per compaction.
fn state(env: &TestEnv, hooks: Vec<Arc<dyn Hooks>>) -> SessionState {
    let eng = Arc::new(ScriptedEngine::new(vec![
        ScriptTurn::text("SUMMARY ONE"),
        ScriptTurn::text("SUMMARY TWO"),
    ]));
    let engine: Arc<dyn Engine> = eng;
    build(env, engine, hooks)
}

fn ui() -> oxa_proto::HeadlessUi {
    oxa_proto::HeadlessUi::default()
}

/// Append a user message the way `Session::prompt` does.
fn say(st: &mut SessionState, text: &str) {
    push(st, WireMsg::user(text));
}

/// Append one wire message of any role as a `Msg` entry.
fn push(st: &mut SessionState, wire: WireMsg) {
    append_entry(
        st,
        EntryBody::Msg {
            wire,
            parts: Vec::new(),
            tool: None,
        },
    )
    .expect("append message");
}

/// Compact now, as a manual `/compact` does.
fn compact(st: &mut SessionState) {
    compact_now(
        st,
        None,
        CompactReason::Manual,
        &mut VecSink::default(),
        &mut ui(),
    )
    .expect("compact");
}

/// How many compaction entries the session tree holds.
fn compactions(st: &SessionState) -> usize {
    st.tree
        .entries()
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Compaction { .. }))
        .count()
}

/// Give the wire older history and a newest prompt, so that a compaction has something to
/// summarise (an empty wire has no cut point and the compaction is skipped).
fn seed_history(st: &mut SessionState, newest: &str) {
    say(st, "older task");
    push(st, WireMsg::assistant("older work"));
    say(st, newest);
}

/// A hook set that adds one prompt section and continues everywhere.
struct SectionHook;

impl Hooks for SectionHook {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    fn run_command(&self, _name: &str, _args: &str, _ui: &mut dyn Ui) -> Result<(), OxaError> {
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        vec![("hook-policy".to_string(), "HOOK POLICY TEXT".to_string())]
    }
}

/// The prefix section with this name, if it is there.
fn section<'a>(st: &'a SessionState, name: &str) -> Option<&'a str> {
    st.prefix
        .sections
        .iter()
        .find(|(n, _)| n.as_str() == name)
        .map(|(_, text)| text.as_str())
}

/// Whether the wire holds this user text as a message of its own.
fn has_user(st: &SessionState, text: &str) -> bool {
    st.transcript
        .wire()
        .iter()
        .any(|m| m.content.as_deref() == Some(text))
}

// ---------------------------------------------------------------------------------------------
// Tests

/// The goal section and the goal-continue prompt both survive two compactions.
#[test]
fn goal_section_and_continue_survive_two_compactions() {
    let env = TestEnv::new();
    let mut st = state(&env, Vec::new());
    let objective = "Ship the compaction regression tests.";
    let goal = Goal::new(objective.to_string(), GoalBudget::default(), 1000).expect("goal");
    st.goal = Some(Arc::new(oxa_core::goal::GoalRuntime::new(goal)));
    open_epoch(&mut st, "goal", true, &mut VecSink::default()).expect("goal epoch");
    let expected = goal_start(objective);
    assert!(
        section(&st, "goal").is_some_and(|t| t.contains(expected.as_str())),
        "goal section in the prefix"
    );

    // The goal-continue prompt is the newest message, so the keep_tokens: 1 cut lands on it and
    // it is kept. A manual compaction moves the cut forward to a valid point, so a trailing
    // assistant reply after the prompt would be summarised (only a mid-run cut is clamped to
    // the last user message).
    say(&mut st, "the task");
    push(&mut st, WireMsg::assistant("working"));
    push(&mut st, WireMsg::user(GOAL_CONTINUE));
    compact(&mut st);
    assert!(
        section(&st, "goal").is_some_and(|t| t.contains(expected.as_str())),
        "goal section after one compaction"
    );
    assert!(
        has_user(&st, GOAL_CONTINUE),
        "goal-continue prompt after one compaction"
    );

    // The next continuation prompt is again the newest message when the second compaction runs.
    push(&mut st, WireMsg::assistant("more work"));
    push(&mut st, WireMsg::user(GOAL_CONTINUE));
    compact(&mut st);
    assert!(
        section(&st, "goal").is_some_and(|t| t.contains(expected.as_str())),
        "goal section after two compactions"
    );
    assert!(
        has_user(&st, GOAL_CONTINUE),
        "goal-continue prompt after two compactions"
    );
}

/// A queued follow-up is not touched by a compaction.
#[test]
fn queued_follow_up_survives_compaction() {
    let env = TestEnv::new();
    let mut st = state(&env, Vec::new());
    st.shared
        .queue
        .lock()
        .expect("queue")
        .follow_up
        .push("follow up later".to_string());

    seed_history(&mut st, "first prompt");
    compact(&mut st);
    assert_eq!(compactions(&st), 1, "first compaction ran");
    push(&mut st, WireMsg::assistant("more work"));
    say(&mut st, "second prompt");
    compact(&mut st);
    assert_eq!(compactions(&st), 2, "second compaction ran");

    let queue = st.shared.queue.lock().expect("queue");
    assert_eq!(
        queue.follow_up,
        vec!["follow up later".to_string()],
        "queued follow-up after two compactions"
    );
    assert!(
        !has_user(&st, "follow up later"),
        "a queued follow-up is not in the wire"
    );
}

/// A hook prompt section stays in the prefix after compaction.
#[test]
fn hook_prompt_sections_survive_compaction() {
    let env = TestEnv::new();
    let hooks: Vec<Arc<dyn Hooks>> = vec![Arc::new(SectionHook)];
    let mut st = state(&env, hooks);
    open_epoch(&mut st, "hooks", true, &mut VecSink::default()).expect("hook epoch");
    let before = section(&st, "hook-policy").expect("hook section in the prefix");
    assert!(before.contains("HOOK POLICY TEXT"), "{before}");

    seed_history(&mut st, "newest prompt");
    compact(&mut st);
    assert_eq!(compactions(&st), 1, "compaction ran");

    let after = section(&st, "hook-policy").expect("hook section after compaction");
    assert!(after.contains("HOOK POLICY TEXT"), "{after}");
}

// ---------------------------------------------------------------------------------------------
// C2: the skills active before a compaction are listed in its summary

const LINT: &str = "/skills/lint/SKILL.md";

/// The discovered skill a `<skill>` tag names: a tag only counts when the session has it.
fn lint_skill() -> Skill {
    Skill {
        name: "lint".to_string(),
        description: "Lint the tree".to_string(),
        path: LINT.into(),
        base_dir: "/skills/lint".into(),
        source: ResourceSource::User,
        disable_model_invocation: false,
    }
}

/// The user message a loaded skill is sent as (`oxa-resources`, `expand_skill_command`).
fn skill_message(name: &str, location: &str) -> String {
    format!("<skill name=\"{name}\" location=\"{location}\">\nReferences are relative to /skills.\n\nbody\n</skill>")
}

/// The summary user message at the head of the wire.
fn summary_head(st: &SessionState) -> String {
    st.transcript.wire()[0].content.clone().unwrap_or_default()
}

/// A skill loaded before the cut is summarised away, and the summary lists it.
#[test]
fn skill_loaded_before_cut_is_listed_after_compaction() {
    let env = TestEnv::new();
    let mut st = state(&env, Vec::new());
    st.resources.skills = vec![lint_skill()];
    say(&mut st, &skill_message("lint", LINT));
    push(&mut st, WireMsg::assistant("linted"));
    say(&mut st, "the next task");
    push(&mut st, WireMsg::assistant("done"));
    compact(&mut st);

    let head = summary_head(&st);
    assert!(head.contains("<active-skills>"), "{head}");
    assert!(head.contains(&format!("lint {LINT}")), "{head}");
    assert!(head.contains("Re-read these files before continuing."), "{head}");
}

/// The listed skill is carried by the compaction details into the next compaction, where its
/// message is no longer in the wire, and it is listed once.
#[test]
fn active_skills_carry_across_repeated_compaction() {
    let env = TestEnv::new();
    let mut st = state(&env, Vec::new());
    st.resources.skills = vec![lint_skill()];
    say(&mut st, &skill_message("lint", LINT));
    push(&mut st, WireMsg::assistant("linted"));
    say(&mut st, "the first task");
    push(&mut st, WireMsg::assistant("done one"));
    compact(&mut st);
    assert!(summary_head(&st).contains(LINT), "after one compaction");

    say(&mut st, "the second task");
    push(&mut st, WireMsg::assistant("done two"));
    compact(&mut st);

    let head = summary_head(&st);
    assert!(head.contains(&format!("lint {LINT}")), "{head}");
    assert_eq!(head.matches(LINT).count(), 1, "listed once: {head}");
}

/// A prompt that merely quotes a `<skill>` tag does not make that skill active: only a skill the
/// session discovered is listed.
#[test]
fn quoted_skill_tag_is_not_an_active_skill() {
    let env = TestEnv::new();
    let mut st = state(&env, Vec::new());
    st.resources.skills = vec![lint_skill()];
    // Even a discovered skill is not active when its canonical wrapper is merely quoted.
    say(
        &mut st,
        &format!("explain this form: {}", skill_message("lint", LINT)),
    );
    push(&mut st, WireMsg::assistant("explained"));
    say(&mut st, "the next task");
    push(&mut st, WireMsg::assistant("done"));
    compact(&mut st);

    let head = summary_head(&st);
    assert!(!head.contains("<active-skills>"), "{head}");
    assert!(!head.contains(LINT), "{head}");
}

/// Skill reactivation changes model context and is therefore explicitly opt-in.
#[test]
fn skill_reactivation_is_off_by_default() {
    let env = TestEnv::new();
    let mut st = state(&env, Vec::new());
    st.settings = Settings::default();
    st.resources.skills = vec![lint_skill()];
    say(&mut st, &skill_message("lint", LINT));
    push(&mut st, WireMsg::assistant("linted"));
    say(&mut st, "the next task");
    push(&mut st, WireMsg::assistant("done"));
    compact(&mut st);

    let head = summary_head(&st);
    assert!(!head.contains("<active-skills>"), "{head}");
}

#[test]
fn trusted_project_cannot_enable_skill_reactivation() {
    let env = TestEnv::new();
    let mut st = state(&env, Vec::new());
    st.settings = Settings::from_values(
        Value::Object(Default::default()),
        serde_json::json!({"compaction": {"reactivateSkills": true}}),
    );
    st.resources.skills = vec![lint_skill()];
    say(&mut st, &skill_message("lint", LINT));
    push(&mut st, WireMsg::assistant("linted"));
    say(&mut st, "the next task");
    push(&mut st, WireMsg::assistant("done"));
    compact(&mut st);

    let head = summary_head(&st);
    assert!(!head.contains("<active-skills>"), "{head}");
}

/// The retained skills list shares the summary budget: whole lines are kept while they fit and
/// the rest are counted, so a long history cannot grow the summary without bound.
#[test]
fn retained_skills_stay_within_the_summary_budget() {
    let env = TestEnv::new();
    let mut st = state(&env, Vec::new());
    // Ten discovered skills cannot all fit once the complete block (including its wrapper and
    // instruction) is charged to this deliberately small summary budget.
    st.compact.summary_max = 180;
    let mut skills = Vec::new();
    for i in 0..10 {
        let name = format!("s{i:02}");
        let location = format!("/skills/{name}/SKILL.md");
        skills.push(Skill {
            name: name.clone(),
            description: "Loaded skill".to_string(),
            path: location.clone().into(),
            base_dir: format!("/skills/{name}").into(),
            source: ResourceSource::User,
            disable_model_invocation: false,
        });
        say(&mut st, &skill_message(&name, &location));
        push(&mut st, WireMsg::assistant("loaded"));
    }
    st.resources.skills = skills;
    say(&mut st, "the next task");
    push(&mut st, WireMsg::assistant("done"));
    compact(&mut st);

    let head = summary_head(&st);
    assert!(head.contains("<active-skills>"), "{head}");
    assert!(head.contains("s00 /skills/s00/SKILL.md"), "{head}");
    assert!(!head.contains("s09 /skills/s09/SKILL.md"), "{head}");
    assert!(head.contains("more not listed: summary budget"), "{head}");
}

/// Without a skill in the summarised messages there is no block.
#[test]
fn no_skill_no_block() {
    let env = TestEnv::new();
    let mut st = state(&env, Vec::new());
    say(&mut st, "a plain task");
    push(&mut st, WireMsg::assistant("done"));
    say(&mut st, "the next task");
    push(&mut st, WireMsg::assistant("done too"));
    compact(&mut st);

    let head = summary_head(&st);
    assert!(head.contains("SUMMARY ONE"), "{head}");
    assert!(!head.contains("<active-skills>"), "{head}");
}
