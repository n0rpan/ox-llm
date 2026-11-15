//! C2 acceptance tests: Auto-retry and overflow recovery. Owner: task C2.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::{
    append_entry, backoff_ms, chat_with_retry, run_loop, Queue, RequestOpts, RetryPolicy, RunEnd,
    SessionState, Shared, Stats, Toolset,
};
use oxa_fake::{Fault, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    codes, AgentEvent, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, EntryBody, FrozenPrefix,
    HeadlessUi, Mode, OxaError, Policy, StreamEvent, ThinkingLevel, TurnKind, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore, Transcript, Tree};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------------
// Harness

/// A session over a scripted engine. `settings` is the user layer of the settings (retry knobs).
fn state(
    env: &TestEnv,
    turns: Vec<ScriptTurn>,
    settings: serde_json::Value,
) -> (SessionState, Arc<ScriptedEngine>) {
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let cwd = env.work();
    let sref = store.create(&cwd, None, None).expect("create session");
    let info = engine.info(None).expect("engine info");
    let mut prefix = FrozenPrefix {
        sections: vec![("preamble".to_string(), "You are a test agent.".to_string())],
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
    });
    let st = SessionState {
        engine,
        store,
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()) as Box<dyn Policy>,
        sandbox: Arc::new(NoSandbox),
        settings: Settings::from_values(settings, json!({})),
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
        compact: CompactPolicy {
            soft: u32::MAX,
            hard: u32::MAX,
            keep_tokens: 4000,
            summary_max: 1024,
            enabled: true,
        },
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
    };
    (st, eng)
}

fn say(st: &mut SessionState, text: &str) {
    append_entry(
        st,
        EntryBody::Msg {
            wire: WireMsg::user(text),
            parts: Vec::new(),
        },
    )
    .expect("append user");
}

/// An earlier exchange, so that an overflow compaction has something to summarise: the
/// current prompt itself is never folded (W5).
fn earlier_exchange(st: &mut SessionState) {
    st.compact.keep_tokens = 1;
    say(st, "an earlier question");
    append_entry(
        st,
        EntryBody::Msg {
            wire: WireMsg::assistant("an earlier answer"),
            parts: Vec::new(),
        },
    )
    .expect("append assistant");
}

fn opts() -> RequestOpts {
    RequestOpts {
        kind: TurnKind::Chat,
        max_tokens: None,
        thinking: None,
        background: false,
        snapshot_keep: false,
        extra: Vec::new(),
        events: true,
        idem_key: String::new(),
    }
}

/// A refused connection: `engine_down` before any data byte.
fn refuse() -> ScriptTurn {
    ScriptTurn::fault(Fault::Refuse)
}

/// Retry knobs for tests: a 1 ms base keeps the waits short.
fn fast_retry(max_retries: u32) -> serde_json::Value {
    json!({"retry": {"enabled": true, "maxRetries": max_retries, "baseDelayMs": 1, "maxAgentDelayMs": 5}})
}

/// The `AutoRetryStart` events as (attempt, max_attempts, delay_ms).
fn retry_starts(events: &[AgentEvent]) -> Vec<(u32, u32, u64)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
                ..
            } => Some((*attempt, *max_attempts, *delay_ms)),
            _ => None,
        })
        .collect()
}

fn retry_ends(events: &[AgentEvent]) -> Vec<(bool, u32, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::AutoRetryEnd {
                success,
                attempt,
                final_error,
            } => Some((*success, *attempt, final_error.clone())),
            _ => None,
        })
        .collect()
}

fn text_of(t: &AssistantTurn) -> String {
    t.msg.content.clone().unwrap_or_default()
}

/// Wraps the scripted engine and cancels the retry wait during the first request.
struct CancelRetryOnFirst {
    inner: Arc<dyn Engine>,
    shared: Arc<Shared>,
    calls: AtomicUsize,
}

impl Engine for CancelRetryOnFirst {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
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
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.shared.cancel_retry.cancel();
        }
        self.inner.chat(r, cancel, on)
    }
}

// ---------------------------------------------------------------------------------------------
// Acceptance tests

/// pi's delays
#[test]
fn backoff_table() {
    let p = RetryPolicy {
        enabled: true,
        max_retries: 3,
        base_delay_ms: 2000,
        max_delay_ms: 60000,
    };
    let table = [1u32, 2, 3, 4, 5, 6].map(|a| backoff_ms(a, &p));
    assert_eq!(
        table,
        [2000, 4000, 8000, 16000, 32000, 60000],
        "base * 2^(attempt-1), capped at max"
    );
    assert_eq!(backoff_ms(0, &p), 2000, "attempt 0 behaves like attempt 1");
    assert_eq!(
        backoff_ms(u32::MAX, &p),
        60000,
        "huge attempts saturate instead of overflowing"
    );

    let low_cap = RetryPolicy {
        max_delay_ms: 1000,
        ..p
    };
    assert_eq!(
        backoff_ms(1, &low_cap),
        1000,
        "the cap applies to the first delay too"
    );
}

/// run continues to settle as failed
#[test]
fn abort_retry_cancels_wait_only() {
    let env = TestEnv::new();
    let (mut st, eng) = state(&env, vec![refuse(), ScriptTurn::text("ok")], json!({}));
    // The first request is refused; during it the retry wait is cancelled (`abort_retry`).
    let wrapper = CancelRetryOnFirst {
        inner: st.engine.clone(),
        shared: st.shared.clone(),
        calls: AtomicUsize::new(0),
    };
    st.engine = Arc::new(wrapper);
    say(&mut st, "hi");

    let started = Instant::now();
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let end = run_loop(&mut st, &mut sink, &mut ui).expect("run_loop");
    assert!(
        started.elapsed() < Duration::from_millis(1900),
        "the 2000 ms wait is cut short by abort_retry"
    );
    match end {
        RunEnd::Failed(e) => assert_eq!(
            e.code,
            codes::ENGINE_DOWN,
            "the run settles as failed with the request error"
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(eng.requests().len(), 1, "no retry after the wait was cancelled");
    assert_eq!(eng.remaining(), 1, "the second scripted turn was not consumed");
    assert_eq!(
        retry_ends(&sink.0),
        vec![(false, 1, Some("Retry cancelled".to_string()))]
    );
    assert!(!st.shared.cancel.is_cancelled(), "the run token is untouched");
    assert!(
        !st.shared.cancel_retry.is_cancelled(),
        "the retry cancel is consumed"
    );

    // Only the wait was cancelled: the next call goes through.
    let got =
        chat_with_retry(&mut st, &opts(), &mut VecSink::default(), &mut ui).expect("next call succeeds");
    assert_eq!(text_of(&got), "ok");
}

/// second overflow is returned
#[test]
fn overflow_one_compact_and_retry() {
    let env = TestEnv::new();
    // Overflow, then the compaction summary, then overflow again: the second overflow is returned.
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::fault(Fault::Overflow),
            ScriptTurn::text("summary of the session"),
            ScriptTurn::fault(Fault::Overflow),
        ],
        json!({}),
    );
    earlier_exchange(&mut st);
    say(&mut st, "a long prompt");

    let err = chat_with_retry(
        &mut st,
        &opts(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect_err("second overflow is returned");
    assert_eq!(err.code, codes::CONTEXT_OVERFLOW);

    let reqs = eng.requests();
    assert_eq!(reqs.len(), 3, "overflow, one compaction request, one retry");
    assert_eq!(
        reqs[1].kind, "compaction",
        "the recovery compacts before retrying"
    );
    assert_eq!(eng.remaining(), 0);
}

/// the recovery retry succeeds when compaction made the wire fit
#[test]
fn overflow_recovers_after_one_compaction() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::fault(Fault::Overflow),
            ScriptTurn::text("summary of the session"),
            ScriptTurn::text("answer"),
        ],
        json!({}),
    );
    earlier_exchange(&mut st);
    say(&mut st, "a long prompt");

    let turn = chat_with_retry(
        &mut st,
        &opts(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect("recovered");
    assert_eq!(text_of(&turn), "answer");
    let kinds: Vec<String> = eng.requests().iter().map(|r| r.kind.clone()).collect();
    assert_eq!(kinds, vec!["chat", "compaction", "chat"]);
}

/// exactly one request
#[test]
fn dropped_stream_after_output_no_duplicate_body() {
    let env = TestEnv::new();
    // Three bytes of text arrive, then the stream breaks. The turn after it must stay unused.
    let cut = ScriptTurn {
        chunk: 1,
        fault: Some(Fault::DropAt { bytes: 3 }),
        ..ScriptTurn::text("hello")
    };
    let (mut st, eng) = state(&env, vec![cut, ScriptTurn::text("ok")], fast_retry(3));
    say(&mut st, "hi");

    let mut sink = VecSink::default();
    let err = chat_with_retry(&mut st, &opts(), &mut sink, &mut HeadlessUi::default())
        .expect_err("cut reply is returned");
    assert_eq!(err.code, codes::STREAM_BROKEN);
    assert_eq!(eng.requests().len(), 1, "exactly one request");
    assert_eq!(eng.remaining(), 1, "no second body was sent");
    assert!(retry_starts(&sink.0).is_empty(), "no retry was announced");
}

/// same key
#[test]
fn retry_before_data_reuses_idem_key() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![refuse(), refuse(), ScriptTurn::text("ok")],
        fast_retry(3),
    );
    say(&mut st, "hi");

    let turn = chat_with_retry(
        &mut st,
        &opts(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect("third attempt");
    assert_eq!(text_of(&turn), "ok");

    let reqs = eng.requests();
    assert_eq!(reqs.len(), 3, "refused twice, then answered");
    assert!(!reqs[0].idem_key.is_empty(), "an idempotency key is always sent");
    assert!(
        reqs.iter().all(|r| r.idem_key == reqs[0].idem_key),
        "every attempt carries the same key"
    );
    assert_eq!(
        st.transcript.wire().len(),
        1,
        "retries push nothing to the wire; only the user message is there"
    );
}

/// AutoRetryStart/End fields
#[test]
fn retry_events() {
    let env = TestEnv::new();
    let (mut st, _eng) = state(&env, vec![refuse(), ScriptTurn::text("ok")], fast_retry(3));
    say(&mut st, "hi");

    let mut sink = VecSink::default();
    chat_with_retry(&mut st, &opts(), &mut sink, &mut HeadlessUi::default()).expect("retried");

    let starts = retry_starts(&sink.0);
    assert_eq!(
        starts,
        vec![(1, 3, 1)],
        "attempt 1 of maxRetries 3, delay base * 2^0 = 1 ms"
    );
    let start_msg = sink
        .0
        .iter()
        .find_map(|e| match e {
            AgentEvent::AutoRetryStart { error_message, .. } => Some(error_message.clone()),
            _ => None,
        })
        .expect("start event");
    assert!(
        start_msg.contains("connection refused"),
        "the error that caused the retry is named: {start_msg}"
    );
    assert_eq!(
        retry_ends(&sink.0),
        vec![(true, 1, None)],
        "success after one retry"
    );

    // Exhausted retries: the failure is reported with the attempt count.
    let env2 = TestEnv::new();
    let (mut st2, eng2) = state(&env2, vec![refuse(), refuse(), refuse(), refuse()], fast_retry(3));
    say(&mut st2, "hi");
    let mut sink2 = VecSink::default();
    let err =
        chat_with_retry(&mut st2, &opts(), &mut sink2, &mut HeadlessUi::default()).expect_err("exhausted");
    assert_eq!(err.code, codes::ENGINE_DOWN);
    assert_eq!(eng2.requests().len(), 4, "one request plus maxRetries");
    assert_eq!(retry_starts(&sink2.0).len(), 3);
    let ends = retry_ends(&sink2.0);
    assert_eq!(ends.len(), 1);
    assert!(!ends[0].0 && ends[0].1 == 3, "failed after three attempts");
    assert!(
        ends[0].2.as_deref().unwrap_or("").contains("connection refused"),
        "final error is kept"
    );
}

/// auto_retry false
#[test]
fn disabled_retry() {
    // Through the settings...
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![refuse(), ScriptTurn::text("ok")],
        json!({"retry": {"enabled": false, "baseDelayMs": 1}}),
    );
    say(&mut st, "hi");
    let mut sink = VecSink::default();
    let err = chat_with_retry(&mut st, &opts(), &mut sink, &mut HeadlessUi::default()).expect_err("no retry");
    assert_eq!(err.code, codes::ENGINE_DOWN);
    assert_eq!(eng.requests().len(), 1);
    assert!(retry_starts(&sink.0).is_empty());

    // ...and through the session switch (`set_auto_retry(false)`).
    let env2 = TestEnv::new();
    let (mut st2, eng2) = state(&env2, vec![refuse(), ScriptTurn::text("ok")], fast_retry(3));
    st2.auto_retry = false;
    say(&mut st2, "hi");
    chat_with_retry(
        &mut st2,
        &opts(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect_err("no retry");
    assert_eq!(eng2.requests().len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Further cases

/// Only 5xx and 429 are retried for an HTTP answer; 4xx fails at once.
#[test]
fn http_status_classes() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::fault(Fault::Http {
                status: 400,
                body: "bad".into(),
            }),
            ScriptTurn::text("ok"),
        ],
        fast_retry(3),
    );
    say(&mut st, "hi");
    let err = chat_with_retry(
        &mut st,
        &opts(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect_err("400 is final");
    assert_eq!(err.code, codes::ENGINE_HTTP);
    assert_eq!(eng.requests().len(), 1);

    let env2 = TestEnv::new();
    let (mut st2, eng2) = state(
        &env2,
        vec![
            ScriptTurn::fault(Fault::Http {
                status: 503,
                body: "busy".into(),
            }),
            ScriptTurn::text("ok"),
        ],
        fast_retry(3),
    );
    say(&mut st2, "hi");
    let turn = chat_with_retry(
        &mut st2,
        &opts(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect("503 is retried");
    assert_eq!(text_of(&turn), "ok");
    assert_eq!(eng2.requests().len(), 2);
}

/// An aborted run is never retried and announces no retry.
#[test]
fn aborted_is_never_retried() {
    let env = TestEnv::new();
    let (mut st, eng) = state(&env, vec![refuse(), ScriptTurn::text("ok")], fast_retry(3));
    say(&mut st, "hi");
    st.shared.cancel.cancel();

    let mut sink = VecSink::default();
    let err = chat_with_retry(&mut st, &opts(), &mut sink, &mut HeadlessUi::default()).expect_err("aborted");
    assert_eq!(err.code, codes::ABORTED);
    assert_eq!(eng.requests().len(), 1, "no retry after an abort");
    assert!(retry_starts(&sink.0).is_empty());
}

/// A stream that breaks before any data is retried, but a summary request (no events) that
/// breaks after data is not, because the probe sees the data even when events are off.
#[test]
fn quiet_request_after_data_is_not_replayed() {
    let env = TestEnv::new();
    let cut = ScriptTurn {
        chunk: 1,
        fault: Some(Fault::DropAt { bytes: 2 }),
        ..ScriptTurn::text("hello")
    };
    let (mut st, eng) = state(&env, vec![cut, ScriptTurn::text("ok")], fast_retry(3));
    say(&mut st, "hi");
    let quiet = RequestOpts {
        events: false,
        ..opts()
    };
    let err = chat_with_retry(
        &mut st,
        &quiet,
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect_err("cut");
    assert_eq!(err.code, codes::STREAM_BROKEN);
    assert_eq!(eng.requests().len(), 1, "no duplicate body");
}

/// The run's cancel token wins over a pending retry: the wait ends as aborted.
#[test]
fn abort_during_wait_is_aborted() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![refuse(), ScriptTurn::text("ok")],
        json!({"retry": {"baseDelayMs": 60000}}),
    );
    say(&mut st, "hi");
    let shared = st.shared.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        shared.cancel.cancel();
    });
    let started = Instant::now();
    let err = chat_with_retry(
        &mut st,
        &opts(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect_err("aborted");
    canceller.join().expect("canceller");
    assert_eq!(err.code, codes::ABORTED);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the 60 s wait is interrupted"
    );
    assert_eq!(eng.requests().len(), 1);
}

/// with nothing before the prompt to summarise, the overflow is returned without a retry
#[test]
fn overflow_with_nothing_to_summarise_is_returned() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![ScriptTurn::fault(Fault::Overflow), ScriptTurn::text("unused")],
        json!({}),
    );
    say(&mut st, "a long prompt");

    let err = chat_with_retry(
        &mut st,
        &opts(),
        &mut VecSink::default(),
        &mut HeadlessUi::default(),
    )
    .expect_err("the overflow is returned");
    assert_eq!(err.code, codes::CONTEXT_OVERFLOW);
    assert_eq!(eng.requests().len(), 1, "the same bytes are not sent twice");
    assert_eq!(eng.remaining(), 1);
}

/// F3 / S2 (fit pass): `engine_busy` (the controller's 503 queue-full, also as an in-stream
/// error after early headers) is retried with the wait it names (`retry after <n> s`), capped
/// by the policy's longest delay; a 4xx error event after queued comments is final (no retry,
/// and the queued comments do not count as data).
#[test]
fn fit_engine_busy_waits_as_told_and_4xx_error_event_is_final() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::fault(Fault::Busy {
                retry_after: Some(30),
            }),
            ScriptTurn::text("ok"),
        ],
        fast_retry(3),
    );
    say(&mut st, "hi");
    let mut sink = VecSink::default();
    let turn = chat_with_retry(&mut st, &opts(), &mut sink, &mut HeadlessUi::default()).expect("retried");
    assert_eq!(text_of(&turn), "ok");
    assert_eq!(eng.requests().len(), 2);
    // `retry after 30 s` capped at maxAgentDelayMs (5 ms in `fast_retry`).
    assert_eq!(retry_starts(&sink.0), vec![(1, 3, 5)]);

    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![
            ScriptTurn::fault(Fault::StreamError {
                queued: 2,
                status: 400,
                error_type: "invalid_request_error".into(),
                event: true,
            }),
            ScriptTurn::text("unused"),
        ],
        fast_retry(3),
    );
    say(&mut st, "hi");
    let mut sink = VecSink::default();
    let err =
        chat_with_retry(&mut st, &opts(), &mut sink, &mut HeadlessUi::default()).expect_err("a 400 is final");
    assert_eq!(err.code, codes::ENGINE_HTTP);
    assert_eq!(eng.requests().len(), 1);
    assert!(retry_starts(&sink.0).is_empty());
}

/// #141: a looping reply is stopped, the run ends Failed, the Shown entry carries the partial
/// text and the notice, and the shown text is never sent back to the engine.
#[test]
fn looping_reply_ends_failed_with_notice_and_is_not_sent_back() {
    let env = TestEnv::new();
    let para: String = (0..40).map(|i| format!("word{} ", (i * 7) % 97)).collect();
    let looping = format!("Start.\n\n{}", para.repeat(8));
    let mut turn = ScriptTurn::text(&looping);
    turn.chunk = 16;
    let (mut st, eng) = state(&env, vec![turn, ScriptTurn::text("fine")], json!({}));
    say(&mut st, "hi");

    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let end = run_loop(&mut st, &mut sink, &mut ui).expect("run_loop");
    match end {
        RunEnd::Failed(e) => assert!(e.message.contains("repetition loop"), "{e}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(!st.shared.cancel.is_cancelled(), "our own stop clears the flag");

    let shown: Vec<String> = st
        .tree
        .entries()
        .iter()
        .filter_map(|e| match &e.body {
            EntryBody::Shown { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(shown.len(), 1);
    assert!(shown[0].starts_with("Start."), "partial reply first");
    assert!(
        shown[0].contains("\n\nResponse stopped: the model fell into a repetition loop"),
        "notice after a blank line: {}",
        shown[0]
    );
    assert!(shown[0].contains("Rephrase the request"), "hint included");

    // The next request does not carry the looping text.
    say(&mut st, "again");
    let end = run_loop(&mut st, &mut sink, &mut ui).expect("run_loop 2");
    assert_eq!(end, RunEnd::Stopped);
    let sent = eng.requests().last().cloned().expect("request").messages;
    assert!(sent.iter().all(
        |m| !m.content.as_deref().unwrap_or("").contains("repetition loop")
            && !m.content.as_deref().unwrap_or("").contains("word0 ")
    ));
}
