//! Fit-review fixes in the core, against the HTTP fake where it models the engine:
//! F12 (summary priority per reason), S4 (`compaction.modelOverrides`), F4 (capabilities
//! re-read after a model load), F6 (a reply cut by a full context is an overflow), F11
//! (`snap_pos`, the H4 hint only when advertised, the leaf-move cost notice) and S2 (queued
//! comments and in-stream errors at the session level).

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{
    cache_bytes, FakeCaps, FakeModel, FakeOpts, FakeServer, Fault, Recorded, ScriptTurn, TestEnv,
};
use oxa_perm::ModePolicy;
use oxa_proto::{
    shown_why, AgentEvent, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, EntryBody,
    HeadlessUi, Level, OxaError, Role, StreamEvent, TurnKind, VecSink,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn open(env: &TestEnv, engine: Arc<dyn Engine>, settings: &str) -> Session {
    let dirs = Dirs::under(env.root());
    let path = dirs.settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, settings).unwrap();
    // Rebase identity assertions below deliberately select note-free prompt bytes.
    let settings =
        Settings::load(&dirs, &env.work(), false).with_override("oxa.envNote", serde_json::json!(false));
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    Session::open(
        Deps {
            engine,
            store,
            tools: Vec::new(),
            hooks: Vec::new(),
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings,
            dirs,
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
    .expect("open")
}

fn http(server: &FakeServer) -> Arc<dyn Engine> {
    Arc::new(oxa_engine::HttpEngine::new(&server.url(), None).expect("engine"))
}

fn prompt(s: &mut Session, text: &str) -> Vec<AgentEvent> {
    let mut sink = VecSink::default();
    s.prompt(
        text.to_string(),
        Vec::new(),
        &mut sink,
        &mut HeadlessUi::default(),
    )
    .expect("prompt");
    sink.0
}

fn notices(events: &[AgentEvent], level: Level) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Notice { level: l, text } if *l == level => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// `(kind, background)` of every recorded request.
fn kinds(reqs: &[Recorded]) -> Vec<(String, bool)> {
    reqs.iter().map(|r| (r.kind.clone(), r.background)).collect()
}

/// The frozen prefix bytes of a session opened with these settings.
fn prefix_len(env: &TestEnv) -> u32 {
    let eng: Arc<dyn Engine> = Arc::new(oxa_fake::ScriptedEngine::new(Vec::new()));
    let s = open(env, eng, "{}");
    let p = &s.inner().prefix;
    cache_bytes(&p.system_text(), &p.tools_json, &[]).len() as u32
}

const FAST_RETRY: &str = r#""retry": {"baseDelayMs": 1, "maxAgentDelayMs": 5}"#;

// ---------------------------------------------------------------------------------------------
// F12

/// Only the threshold summary (and the idle prefill) is `X-Oxe-Priority: background`.
#[test]
fn threshold_summary_and_prefill_are_background() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![ScriptTurn::text("r1"), ScriptTurn::text("SUM")],
        FakeOpts::default(),
    )
    .unwrap();
    let mut s = open(
        &env,
        http(&server),
        r#"{"compaction": {"keepRecentTokens": 1}, "oxa": {"compaction": {"softTokens": 10}}}"#,
    );
    prompt(&mut s, "one");
    assert_eq!(
        kinds(&server.requests()),
        vec![
            ("chat".to_string(), false),
            ("compaction".to_string(), true),
            ("prefill".to_string(), true),
        ]
    );
}

/// `/compact`, overflow recovery and a branch summary are foreground (the user waits).
#[test]
fn manual_overflow_and_branch_summaries_are_foreground() {
    let env = TestEnv::new();
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("r1"),
            ScriptTurn::fault(Fault::Overflow),
            ScriptTurn::text("SUM-O"),
            ScriptTurn::text("r2"),
            ScriptTurn::text("SUM-M"),
            ScriptTurn::text("BRANCH"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let mut s = open(
        &env,
        http(&server),
        &format!(r#"{{"compaction": {{"keepRecentTokens": 1}}, {FAST_RETRY}}}"#),
    );
    prompt(&mut s, "one");
    prompt(&mut s, "two");
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    s.compact(None, &mut sink, &mut ui).expect("compact");
    let target = s
        .inner()
        .tree
        .entries()
        .iter()
        .find(|e| matches!(&e.body, EntryBody::Msg { wire, .. } if wire.content.as_deref() == Some("two")))
        .expect("the second prompt")
        .id
        .clone();
    s.navigate(&target, true, None, &mut sink, &mut ui)
        .expect("navigate");

    let summaries: Vec<(String, bool)> = kinds(&server.requests())
        .into_iter()
        .filter(|(k, _)| k == "compaction" || k == TurnKind::BranchSummary.as_str())
        .collect();
    assert_eq!(
        summaries,
        vec![
            ("compaction".to_string(), false),
            ("compaction".to_string(), false),
            (TurnKind::BranchSummary.as_str().to_string(), false),
        ]
    );
    assert!(kinds(&server.requests())
        .iter()
        .filter(|(k, _)| k == "prefill")
        .all(|(_, bg)| *bg));
}

// ---------------------------------------------------------------------------------------------
// S4

/// The session model's `compaction.modelOverrides` entry applies its values; one naming
/// another model for the summary is ignored with ONE warning per session and epoch, and the
/// summary runs on the session model.
#[test]
fn model_override_to_another_model_warns_once_and_uses_the_session_model() {
    let env = TestEnv::new();
    let eng = Arc::new(oxa_fake::ScriptedEngine::new(vec![
        ScriptTurn::text("the first reply"),
        ScriptTurn::text("SUM1"),
        ScriptTurn::text("the second reply"),
        ScriptTurn::text("SUM2"),
    ]));
    let model = eng.model.clone();
    let mut s = open(
        &env,
        eng.clone(),
        &format!(
            r#"{{"compaction": {{"modelOverrides": {{"{model}": {{"keepRecentTokens": 7, "reserveTokens": 999, "model": "other-model"}}, "other-model": {{"keepRecentTokens": 3}}}}}}}}"#
        ),
    );
    assert_eq!(s.inner().compact.keep_tokens, 7);
    let mut warnings = Vec::new();
    for p in ["one", "two"] {
        prompt(&mut s, p);
        let mut sink = VecSink::default();
        s.compact(None, &mut sink, &mut HeadlessUi::default())
            .expect("compact");
        warnings.extend(notices(&sink.0, Level::Warning));
    }
    let overrides: Vec<&String> = warnings.iter().filter(|w| w.contains("modelOverrides")).collect();
    assert_eq!(overrides.len(), 1, "{warnings:?}");
    assert!(overrides[0].contains("other-model"));
    let reqs = eng.requests();
    let summaries: Vec<&str> = reqs
        .iter()
        .filter(|r| r.kind == "compaction")
        .map(|r| r.model.as_str())
        .collect();
    assert_eq!(summaries, vec![model.as_str(), model.as_str()]);
}

/// The string form (`"<model>": "<other>"`) is the same redirect; an entry of another model
/// changes nothing for this session.
#[test]
fn model_override_string_form_and_other_models() {
    let env = TestEnv::new();
    let eng = Arc::new(oxa_fake::ScriptedEngine::new(vec![
        ScriptTurn::text("r1"),
        ScriptTurn::text("SUM"),
    ]));
    let model = eng.model.clone();
    let mut s = open(
        &env,
        eng.clone(),
        &format!(
            r#"{{"compaction": {{"keepRecentTokens": 1, "modelOverrides": {{"{model}": "other-model", "x": {{"keepRecentTokens": 3}}}}}}}}"#
        ),
    );
    assert_eq!(s.inner().compact.keep_tokens, 1);
    prompt(&mut s, "one");
    let mut sink = VecSink::default();
    s.compact(None, &mut sink, &mut HeadlessUi::default())
        .expect("compact");
    assert_eq!(
        notices(&sink.0, Level::Warning)
            .iter()
            .filter(|w| w.contains("modelOverrides"))
            .count(),
        1
    );
    assert!(eng.requests().iter().all(|r| r.model == model));
}

// ---------------------------------------------------------------------------------------------
// F4

/// Counts `info` calls.
struct Counting {
    inner: Arc<dyn Engine>,
    infos: AtomicUsize,
}

impl Engine for Counting {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.infos.fetch_add(1, Ordering::SeqCst);
        self.inner.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.inner.count_tokens(text)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        self.inner.chat(r, cancel, on)
    }
}

/// A controller without a `capabilities` block and with nothing loaded: `/props` is 503, so
/// `prefill_only` starts false.
fn unloaded(script: Vec<ScriptTurn>) -> FakeServer {
    FakeServer::start(
        script,
        FakeOpts {
            models: vec![FakeModel {
                alias: "fake-model".to_string(),
                cap: 1 << 20,
                max_output: None,
                caps: None,
            }],
            props_unloaded: true,
            ..FakeOpts::default()
        },
    )
    .unwrap()
}

/// F4: the hints are re-read once after the first reply (the model is loaded by then), and not
/// on every request afterwards.
#[test]
fn capabilities_reread_after_first_reply_then_steady() {
    let env = TestEnv::new();
    let server = unloaded(vec![
        ScriptTurn::text("a"),
        ScriptTurn::text("b"),
        ScriptTurn::text("c"),
    ]);
    let eng = Arc::new(Counting {
        inner: http(&server),
        infos: AtomicUsize::new(0),
    });
    let mut s = open(&env, eng.clone(), r#"{"compaction": {"enabled": false}}"#);
    assert!(!s.inner().info.prefill_only, "no hints while nothing is loaded");
    server.set_props_unloaded(false);
    prompt(&mut s, "one");
    assert!(s.inner().info.prefill_only, "re-read after the first reply");
    let n = eng.infos.load(Ordering::SeqCst);
    prompt(&mut s, "two");
    prompt(&mut s, "three");
    assert_eq!(eng.infos.load(Ordering::SeqCst), n, "no info call per request");
}

/// F4: a request that recovered from `engine_busy` (a model load) re-reads the capabilities,
/// also after the one first-reply re-read has been used up.
#[test]
fn capabilities_reread_after_recovering_from_busy() {
    let env = TestEnv::new();
    let server = unloaded(vec![
        ScriptTurn::text("a"),
        ScriptTurn::fault(Fault::Busy { retry_after: Some(1) }),
        ScriptTurn::text("b"),
    ]);
    let mut s = open(
        &env,
        http(&server),
        &format!(r#"{{"compaction": {{"enabled": false}}, {FAST_RETRY}}}"#),
    );
    prompt(&mut s, "one");
    assert!(
        !s.inner().info.prefill_only,
        "still unloaded at the first re-read"
    );
    server.set_props_unloaded(false);
    let ev = prompt(&mut s, "two");
    assert!(ev.iter().any(|e| matches!(e, AgentEvent::AutoRetryStart { .. })));
    assert!(s.inner().info.prefill_only, "re-read after the recovered request");
}

// ---------------------------------------------------------------------------------------------
// F6

/// Sizes for the F6 band: a context `n_ctx` and a history size so that the second prompt is
/// below `n_ctx` but `prompt + max_output` is not, while the summary request (prompt + the
/// summary instruction) still fits.
struct Band {
    n_ctx: u32,
    history: usize,
}

fn band(env: &TestEnv) -> Band {
    let p = prefix_len(env);
    let instr = oxa_compact::prompt::summary_instruction(oxa_compact::prompt::SummaryKind::Initial, None)
        .len() as u32;
    let n_ctx = (p + 9000).max(p + 4 * (instr + 600));
    let out = (n_ctx / 4).min(16384);
    let lower = n_ctx - out + 100;
    let upper = n_ctx - instr - 300;
    assert!(lower < upper);
    let target = (lower + upper) / 2;
    Band {
        n_ctx,
        history: (target - p - 60) as usize,
    }
}

fn shown(s: &Session) -> Vec<(String, &'static str)> {
    s.inner()
        .tree
        .entries()
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::Shown { text, why } => Some((text.clone(), *why)),
            _ => None,
        })
        .collect()
}

/// F6: a reply cut because the context is full is an overflow: it is shown (`cut`), never
/// pushed; the session compacts in the foreground and retries once; front ends see the cut
/// message closed (the `Shown` entry) and a new `MessageStart` before the retry.
#[test]
fn context_full_length_stop_compacts_and_retries() {
    let env = TestEnv::new();
    let b = band(&env);
    let cut_text = "x".repeat(8000);
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("r1"),
            ScriptTurn::text(&cut_text),
            ScriptTurn::text("SUM"),
            ScriptTurn::text("final"),
        ],
        FakeOpts {
            n_ctx: b.n_ctx,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut s = open(
        &env,
        http(&server),
        r#"{"compaction": {"enabled": false, "keepRecentTokens": 1}}"#,
    );
    assert_eq!(s.inner().info.n_ctx, b.n_ctx);
    prompt(&mut s, &"h".repeat(b.history));
    let ev = prompt(&mut s, "short");

    let reqs = server.requests();
    assert_eq!(
        kinds(&reqs),
        vec![
            ("chat".to_string(), false),
            ("chat".to_string(), false),
            ("compaction".to_string(), false),
            ("chat".to_string(), false),
        ]
    );
    let cut = reqs[1].usage.expect("usage");
    assert!(cut.prompt + cut.completion >= b.n_ctx - 1, "{cut:?}");
    let wire = s.inner().transcript.wire();
    assert_eq!(wire.last().and_then(|m| m.content.as_deref()), Some("final"));
    assert!(wire
        .iter()
        .all(|m| !m.content.as_deref().unwrap_or("").starts_with("xxx")));
    let shown = shown(&s);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0].1, shown_why::CUT);
    assert!(shown[0].0.starts_with("xxx"));

    let at_shown = ev
        .iter()
        .position(|e| matches!(e, AgentEvent::EntryAppended { entry } if matches!(entry.body, EntryBody::Shown { .. })))
        .unwrap();
    let starts: Vec<usize> = ev
        .iter()
        .enumerate()
        // The assistant's messages (the user prompt has its own `message_start`, pi json.md).
        .filter(|(_, e)| matches!(e, AgentEvent::MessageStart { message } if message.msg.role == oxa_proto::Role::Assistant))
        .map(|(i, _)| i)
        .collect();
    let compaction = ev
        .iter()
        .position(|e| matches!(e, AgentEvent::CompactionStart { .. }))
        .unwrap();
    assert_eq!(starts.len(), 2, "one per attempt");
    assert!(starts[0] < at_shown && at_shown < starts[1] && starts[1] < compaction);
}

/// #512: the context-full reply is not kept, but its request (a full window of prompt tokens
/// plus the cut reply) still counts in the session's token stats.
#[test]
fn context_full_cut_request_is_counted_in_the_stats() {
    let env = TestEnv::new();
    let b = band(&env);
    let cut_text = "x".repeat(8000);
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("r1"),
            ScriptTurn::text(&cut_text),
            ScriptTurn::text("SUM"),
            ScriptTurn::text("final"),
        ],
        FakeOpts {
            n_ctx: b.n_ctx,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut s = open(
        &env,
        http(&server),
        r#"{"compaction": {"enabled": false, "keepRecentTokens": 1}}"#,
    );
    prompt(&mut s, &"h".repeat(b.history));
    prompt(&mut s, "short");
    let reqs = server.requests();
    let chats: Vec<_> = reqs.iter().filter(|r| r.kind == "chat").collect();
    assert_eq!(chats.len(), 3);
    let cut = chats[1].usage.expect("usage");
    let stats = &s.inner().stats;
    assert_eq!(stats.requests, 3, "r1, the cut request and the retry");
    let prompt_sum: u64 = chats
        .iter()
        .map(|r| u64::from(r.usage.expect("usage").prompt))
        .sum();
    assert_eq!(stats.prompt_tokens, prompt_sum, "cut: {cut:?}");
}

/// F6: a second context-full cut after the compaction is returned like a second overflow.
#[test]
fn second_context_full_cut_is_returned() {
    let env = TestEnv::new();
    let b = band(&env);
    let cut_text = "x".repeat(8000);
    let server = FakeServer::start(
        vec![
            ScriptTurn::text("r1"),
            ScriptTurn::text(&cut_text),
            ScriptTurn::text("SUM"),
            ScriptTurn::text(&cut_text),
        ],
        FakeOpts {
            n_ctx: b.n_ctx,
            ..FakeOpts::default()
        },
    )
    .unwrap();
    let mut s = open(
        &env,
        http(&server),
        r#"{"compaction": {"enabled": false, "keepRecentTokens": 1}}"#,
    );
    prompt(&mut s, "one");
    prompt(&mut s, &"h".repeat(b.history));
    assert_eq!(server.requests().len(), 4);
    let shown = shown(&s);
    assert_eq!(
        shown.iter().map(|(_, w)| *w).collect::<Vec<_>>(),
        vec![shown_why::CUT, shown_why::FAILED]
    );
    let wire = s.inner().transcript.wire();
    assert_eq!(wire.last().map(|m| m.role), Some(Role::User), "nothing pushed");
}

// ---------------------------------------------------------------------------------------------
// F11

fn one_model(caps: FakeCaps, script: Vec<ScriptTurn>) -> FakeServer {
    FakeServer::start(
        script,
        FakeOpts {
            models: vec![FakeModel {
                alias: "fake-model".to_string(),
                cap: 1 << 20,
                max_output: None,
                caps: Some(caps),
            }],
            ..FakeOpts::default()
        },
    )
    .unwrap()
}

/// F11: the idle prefill carries `X-Oxe-Snapshot: keep` only when the engine keeps checkpoints,
/// and the kept position is recorded.
#[test]
fn snapshot_hint_only_when_advertised_and_position_recorded() {
    let env = TestEnv::new();
    let server = one_model(
        FakeCaps {
            snapshot_hint: 0,
            ..FakeCaps::rust_moe()
        },
        vec![ScriptTurn::text("r1"), ScriptTurn::text("SUM")],
    );
    let mut s = open(&env, http(&server), r#"{"compaction": {"keepRecentTokens": 1}}"#);
    prompt(&mut s, "one");
    s.compact(None, &mut VecSink::default(), &mut HeadlessUi::default())
        .expect("compact");
    let prefill = server
        .requests()
        .into_iter()
        .find(|r| r.kind == "prefill")
        .expect("prefill");
    assert!(!prefill.snapshot_keep);
    assert_eq!(s.stats().last_snap_pos, None);

    let env = TestEnv::new();
    let server = one_model(
        FakeCaps::rust_moe(),
        vec![ScriptTurn::text("r1"), ScriptTurn::text("SUM")],
    );
    let mut s = open(&env, http(&server), r#"{"compaction": {"keepRecentTokens": 1}}"#);
    prompt(&mut s, "one");
    s.compact(None, &mut VecSink::default(), &mut HeadlessUi::default())
        .expect("compact");
    let prefill = server
        .requests()
        .into_iter()
        .find(|r| r.kind == "prefill")
        .expect("prefill");
    assert!(prefill.snapshot_keep);
    assert_eq!(s.stats().last_snap_pos, prefill.usage.map(|u| u.prompt));
}

/// An engine that keeps no checkpoint for prefills (`snap_pos: null`, as the fake's
/// `snap_null` fault does for generating requests).
struct SnapNull(oxa_fake::ScriptedEngine);

impl Engine for SnapNull {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.0.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.0.count_tokens(text)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        let mut turn = self.0.chat(r, cancel, on)?;
        if r.max_tokens == 0 {
            if let Some(t) = turn.timings.as_mut() {
                t.snap_pos = None;
            }
        }
        Ok(turn)
    }
}

/// F11: a prefill whose checkpoint was not kept gives one `Info` notice, never an error.
#[test]
fn null_snapshot_is_one_info_notice() {
    let env = TestEnv::new();
    let eng = Arc::new(SnapNull(oxa_fake::ScriptedEngine::new(vec![
        ScriptTurn::text("r1"),
        ScriptTurn::text("SUM"),
    ])));
    let mut s = open(&env, eng, r#"{"compaction": {"keepRecentTokens": 1}}"#);
    prompt(&mut s, "one");
    let mut sink = VecSink::default();
    s.compact(None, &mut sink, &mut HeadlessUi::default())
        .expect("compact");
    let info: Vec<String> = notices(&sink.0, Level::Info)
        .into_iter()
        .filter(|n| n.contains("no checkpoint"))
        .collect();
    assert_eq!(info.len(), 1, "{:?}", sink.0);
    assert!(notices(&sink.0, Level::Error).is_empty());
    assert!(notices(&sink.0, Level::Warning).is_empty());
    assert_eq!(s.stats().last_snap_pos, None);
}

/// The fake's `snap_null` fault on a generating request: the core sends H4 only with idle
/// prefill and the park, so a generating request never sees it and nothing is reported.
#[test]
fn snap_null_on_a_generating_request_is_silent() {
    let env = TestEnv::new();
    let mut turn = ScriptTurn::text("r1");
    turn.fault = Some(Fault::SnapNull {
        reason: "no_boundary_in_last_message".to_string(),
    });
    let server = one_model(FakeCaps::rust_moe(), vec![turn]);
    let mut s = open(&env, http(&server), r#"{"compaction": {"enabled": false}}"#);
    let ev = prompt(&mut s, "one");
    assert!(notices(&ev, Level::Info)
        .iter()
        .all(|n| !n.contains("no checkpoint")));
    assert!(!server.requests()[0].snapshot_keep);
}

/// S3 / F11: a leaf move prints the tokens to prefill and the estimated seconds BEFORE the idle
/// prefill starts.
#[test]
fn leaf_move_notice_precedes_the_prefill() {
    let env = TestEnv::new();
    let server = one_model(
        FakeCaps::rust_moe(),
        vec![ScriptTurn::text("r1"), ScriptTurn::text("r2")],
    );
    let mut s = open(&env, http(&server), r#"{"compaction": {"enabled": false}}"#);
    prompt(&mut s, "one");
    prompt(&mut s, "two");
    let target = s
        .inner()
        .tree
        .entries()
        .iter()
        .find(|e| matches!(&e.body, EntryBody::Msg { wire, .. } if wire.content.as_deref() == Some("two")))
        .unwrap()
        .id
        .clone();
    let mut sink = VecSink::default();
    s.navigate(&target, false, None, &mut sink, &mut HeadlessUi::default())
        .expect("navigate");
    let notice = sink
        .0
        .iter()
        .position(|e| matches!(e, AgentEvent::Notice { text, .. } if text.starts_with("leaf moved")))
        .expect("leaf-move notice");
    let prefill = sink
        .0
        .iter()
        .position(|e| matches!(e, AgentEvent::Prefill { .. }))
        .expect("the idle prefill reports progress");
    assert!(notice < prefill);
    let AgentEvent::Notice { text, .. } = &sink.0[notice] else {
        unreachable!()
    };
    assert!(text.contains("tokens (~") && text.contains(" s)"), "{text}");
}

// ---------------------------------------------------------------------------------------------
// S2

/// S2 (ENG-003) at the session level over HTTP: queued comments become `EngineQueue` events and
/// do not touch the reply; an in-stream 503 after queued comments is retried; an in-stream 400
/// is final; an in-stream `exceed_context_size_error` triggers overflow compaction.
#[test]
fn queued_comments_and_in_stream_errors() {
    let env = TestEnv::new();
    let mut queued = ScriptTurn::text("queued reply");
    queued.fault = Some(Fault::Queued { count: 2, pos: true });
    let stream_error = |status: u16, error_type: &str| {
        ScriptTurn::fault(Fault::StreamError {
            queued: 1,
            status,
            error_type: error_type.to_string(),
            event: true,
        })
    };
    let server = FakeServer::start(
        vec![
            queued,
            stream_error(503, "unavailable_error"),
            ScriptTurn::text("after 503"),
            stream_error(400, "invalid_request_error"),
            stream_error(400, "exceed_context_size_error"),
            ScriptTurn::text("SUM"),
            ScriptTurn::text("after overflow"),
        ],
        FakeOpts::default(),
    )
    .unwrap();
    let mut s = open(
        &env,
        http(&server),
        &format!(r#"{{"compaction": {{"enabled": false, "keepRecentTokens": 1}}, {FAST_RETRY}}}"#),
    );
    let last_reply = |s: &Session| {
        s.inner()
            .transcript
            .wire()
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .and_then(|m| m.content.clone())
    };

    let ev = prompt(&mut s, "one");
    let queue: Vec<(Option<u32>, u32)> = ev
        .iter()
        .filter_map(|e| match e {
            AgentEvent::EngineQueue { pos, wait_s } => Some((*pos, *wait_s)),
            _ => None,
        })
        .collect();
    assert_eq!(
        queue.iter().map(|q| q.0).collect::<Vec<_>>(),
        vec![Some(2), Some(1)]
    );
    assert_eq!(last_reply(&s).as_deref(), Some("queued reply"));

    let ev = prompt(&mut s, "two");
    assert!(ev.iter().any(|e| matches!(e, AgentEvent::AutoRetryStart { .. })));
    assert_eq!(last_reply(&s).as_deref(), Some("after 503"));
    assert_eq!(server.requests().len(), 3);

    let ev = prompt(&mut s, "three");
    assert!(!ev.iter().any(|e| matches!(e, AgentEvent::AutoRetryStart { .. })));
    assert_eq!(server.requests().len(), 4, "a 400 is final");
    assert!(shown(&s).iter().any(|(_, w)| *w == shown_why::FAILED));

    let ev = prompt(&mut s, "four");
    assert!(ev
        .iter()
        .any(|e| matches!(e, AgentEvent::CompactionEnd { result: Some(_), .. })));
    assert_eq!(last_reply(&s).as_deref(), Some("after overflow"));
    let summary = server
        .requests()
        .into_iter()
        .find(|r| r.kind == "compaction")
        .unwrap();
    assert!(!summary.background, "overflow recovery is foreground");
}
