//! #460: on a local model the page size, the output cap and the kept tail follow the measured
//! prefill speed and a target prefill latency per hop (`oxa.tune.hopSeconds`). Without a
//! measurement (or on a remote model, or with the target off) the current values stay.
//! A fake engine reports the prefill timings; no model runs.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    AgentEvent, AssistantTurn, CancelToken, ChatRequest, Engine, EngineInfo, HeadlessUi, OxaError,
    StreamEvent, Timings, VecSink,
};
use oxa_resources::{discover, DiscoverOpts};
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use std::sync::Arc;

/// Prompt tokens of every measured reply: well above the least sample.
const PROMPT_N: u32 = 3000;

/// A scripted engine whose replies report `PROMPT_N` new prompt tokens prefilled at `tok_s`
/// tokens per second (`0` = no prefill time reported, as the plain fake does).
struct Measured {
    chat: Arc<ScriptedEngine>,
    tok_s: f64,
}

impl Engine for Measured {
    fn info(&self, target: Option<&str>) -> Result<EngineInfo, OxaError> {
        self.chat.info(target)
    }
    fn count_tokens(&self, text: &str) -> Result<u32, OxaError> {
        self.chat.count_tokens(text)
    }
    fn chat(
        &self,
        r: &ChatRequest,
        cancel: &CancelToken,
        on: &mut dyn FnMut(StreamEvent),
    ) -> Result<AssistantTurn, OxaError> {
        let mut turn = self.chat.chat(r, cancel, on)?;
        let mut t = turn.timings.unwrap_or_default();
        t.prompt_n = PROMPT_N;
        t.prompt_ms = if self.tok_s > 0.0 {
            f64::from(PROMPT_N) / self.tok_s * 1000.0
        } else {
            0.0
        };
        turn.timings = Some(Timings { ..t });
        Ok(turn)
    }
    fn clear_hint(&self, hint: &str) -> Result<(), OxaError> {
        self.chat.clear_hint(hint)
    }
    fn count_prompt(&self, req: &ChatRequest) -> Result<Option<u64>, OxaError> {
        self.chat.count_prompt(req)
    }
}

fn write_settings(env: &TestEnv, json: &str) {
    let path = Dirs::under(env.root()).settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json).unwrap();
}

/// A session on a 131072-token window whose engine prefills at `tok_s`.
fn open(env: &TestEnv, tok_s: f64, remote: bool) -> (Session, Arc<ScriptedEngine>) {
    let mut chat = ScriptedEngine::new(Vec::new()).with_limits(131_072, 1024);
    if remote {
        chat = chat.with_remote();
    }
    let chat = Arc::new(chat);
    let engine = Arc::new(Measured {
        chat: chat.clone(),
        tok_s,
    });
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), true);
    let opts = DiscoverOpts {
        trusted: true,
        ..Default::default()
    };
    let resources = discover(&dirs, &env.work(), &settings, &opts);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let s = Session::open(
        Deps {
            engine,
            store,
            tools: oxa_tools::builtin_tools(),
            hooks: Vec::new(),
            policy: Box::new(ModePolicy::new()),
            sandbox: Arc::new(NoSandbox),
            settings,
            dirs,
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources,
            model: None,
            thinking: None,
            mode: None,
            session_id: Some("s1".to_string()),
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .expect("open");
    (s, chat)
}

/// A 1000-line file of 40-byte lines (`row NNNN` padded), larger than every first page.
fn big_file(env: &TestEnv) {
    let body: String = (1..=1000)
        .map(|n| format!("row {n:04} {}\n", "x".repeat(30)))
        .collect();
    env.write("work/big.txt", &body);
}

/// One `read` of `big.txt` without offset or limit; its result text. The tool call is the first
/// reply, so its timings are the measurement the read is sized by.
fn read_first_page(s: &mut Session, chat: &ScriptedEngine) -> String {
    chat.push(ScriptTurn::tool("c1", "read", r#"{"path":"big.txt"}"#));
    chat.push(ScriptTurn::text("done"));
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    s.prompt("read it".to_string(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt");
    sink.0
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolExecutionEnd {
                tool_name, result, ..
            } if tool_name == "read" => Some(result.text.clone()),
            _ => None,
        })
        .expect("a read result")
}

#[test]
fn a_slow_prefill_shrinks_page_cap_and_tail_to_the_hop_target() {
    let env = TestEnv::new();
    big_file(&env);
    write_settings(&env, r#"{"oxa": {"tune": {"hopSeconds": 10}}}"#);
    let (mut s, chat) = open(&env, 150.0, false);
    // Before any reply: no measurement, the current values.
    assert_eq!(s.inner().tune.read_page_bytes, 8192);
    assert_eq!(s.inner().compact.keep_tokens, 4000);

    let text = read_first_page(&mut s, &chat);
    // 150 tok/s x 10 s = 1500 tokens = 4500 bytes per hop.
    assert!((s.stats().prefill_tok_s - 150.0).abs() < 1e-6);
    assert_eq!(s.inner().tune.read_page_bytes, 4500);
    assert_eq!(s.inner().tune.output_cap_bytes, 4500);
    assert!(
        text.len() <= 4500,
        "the first page is cut at the hop budget: {}",
        text.len()
    );
    assert!(text.contains("row 0050"));
    assert!(!text.contains("row 0150"), "no line past the hop budget");
    // The tail keeps its 2000-token floor (1500 < 2000).
    assert_eq!(s.inner().compact.keep_tokens, 2000);
}

#[test]
fn a_faster_prefill_gets_a_larger_tail_never_above_the_current_values() {
    let env = TestEnv::new();
    big_file(&env);
    write_settings(&env, r#"{"oxa": {"tune": {"hopSeconds": 10}}}"#);
    let (mut s, chat) = open(&env, 300.0, false);
    read_first_page(&mut s, &chat);
    assert_eq!(s.inner().tune.output_cap_bytes, 9000);
    assert_eq!(
        s.inner().tune.read_page_bytes,
        8192,
        "the page stays at most 8 KiB"
    );
    assert_eq!(s.inner().compact.keep_tokens, 3000);

    let env = TestEnv::new();
    big_file(&env);
    write_settings(&env, r#"{"oxa": {"tune": {"hopSeconds": 10}}}"#);
    let (mut s, chat) = open(&env, 5000.0, false);
    read_first_page(&mut s, &chat);
    assert_eq!(s.inner().tune.output_cap_bytes, 16_384);
    assert_eq!(s.inner().compact.keep_tokens, 4000);
}

#[test]
fn no_measurement_keeps_the_current_values() {
    let env = TestEnv::new();
    big_file(&env);
    write_settings(&env, r#"{}"#);
    let (mut s, chat) = open(&env, 0.0, false);
    let text = read_first_page(&mut s, &chat);
    assert_eq!(s.stats().prefill_tok_s, 0.0);
    assert_eq!(s.inner().tune.read_page_bytes, 8192);
    assert_eq!(s.inner().tune.output_cap_bytes, 16_384);
    assert!(text.contains("row 0150"), "the 8 KiB page");
    assert_eq!(s.inner().compact.keep_tokens, 4000);
}

#[test]
fn the_default_target_applies_to_a_measured_local_model() {
    let env = TestEnv::new();
    big_file(&env);
    write_settings(&env, r#"{}"#);
    let (mut s, chat) = open(&env, 150.0, false);
    read_first_page(&mut s, &chat);
    let hop = oxa_config::HOP_SECONDS_DEFAULT;
    assert!(hop > 0);
    let want = (150 * hop * 3).clamp(2048, 16_384);
    assert_eq!(s.inner().tune.output_cap_bytes, want);
}

#[test]
fn target_off_remote_model_and_explicit_settings_keep_their_values() {
    // hopSeconds 0: off.
    let env = TestEnv::new();
    big_file(&env);
    write_settings(&env, r#"{"oxa": {"tune": {"hopSeconds": 0}}}"#);
    let (mut s, chat) = open(&env, 150.0, false);
    read_first_page(&mut s, &chat);
    assert_eq!(s.inner().tune.read_page_bytes, 8192);
    assert_eq!(s.inner().tune.output_cap_bytes, 16_384);
    assert_eq!(s.inner().compact.keep_tokens, 4000);

    // A remote model: its timings are not a local prefill speed.
    let env = TestEnv::new();
    big_file(&env);
    write_settings(&env, r#"{"oxa": {"tune": {"hopSeconds": 10}}}"#);
    let (mut s, chat) = open(&env, 150.0, true);
    read_first_page(&mut s, &chat);
    assert_eq!(s.inner().tune.output_cap_bytes, 16_384);
    assert_eq!(s.inner().compact.keep_tokens, 4000);

    // An explicit toolOutputCap and keepRecentTokens are the values as they are.
    let env = TestEnv::new();
    big_file(&env);
    write_settings(
        &env,
        r#"{"oxa": {"toolOutputCap": 12000, "tune": {"hopSeconds": 10}}, "compaction": {"keepRecentTokens": 5000}}"#,
    );
    let (mut s, chat) = open(&env, 150.0, false);
    read_first_page(&mut s, &chat);
    assert_eq!(s.inner().tune.output_cap_bytes, 12_000);
    assert_eq!(s.inner().compact.keep_tokens, 5000);
}
