//! T5 (OXA-LOCAL-FIRST): `enable_thinking` by turn kind under `oxa.thinking.policy`. One
//! scripted session (prompt, two tool hops, final reply, second prompt, compaction) over the
//! fake engine's HTTP wire, under each policy; the `chat_template_kwargs` of every request is
//! compared with a golden, and the cache accounting must hold across every toggle.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{cache_bytes, check_all, FakeModel, FakeOpts, FakeServer, Recorded, ScriptTurn, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{rebase_why, AgentEvent, Engine, HeadlessUi, ThinkingLevel, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore};
use serde_json::{json, Value};
use std::sync::Arc;

fn write_settings(env: &TestEnv, json: &str) {
    let path = Dirs::under(env.root()).settings_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, json).unwrap();
}

/// The script: prompt 1 -> read, read, final; prompt 2 -> final; then the compaction summary.
/// After the compaction the core prefills the rebased wire (H6, background): a sixth request.
fn script() -> Vec<ScriptTurn> {
    vec![
        ScriptTurn::tool("c1", "read", r#"{"path":"a.txt"}"#),
        ScriptTurn::tool("c2", "read", r#"{"path":"a.txt"}"#),
        ScriptTurn::text("done 1"),
        ScriptTurn::text("done 2"),
        ScriptTurn::text("## Goal\nsummary"),
    ]
}

struct Run {
    reqs: Vec<Recorded>,
    events: Vec<AgentEvent>,
    /// The test root, which the bodies name (cwd, skill paths).
    root: String,
}

impl Run {
    /// The request bodies with the test root replaced, comparable across runs.
    fn bodies(&self) -> Vec<String> {
        self.reqs
            .iter()
            .map(|q| String::from_utf8_lossy(&q.body).replace(&self.root, "<root>"))
            .collect()
    }
}

fn run(settings: Option<&str>, level: ThinkingLevel, opts: FakeOpts) -> Run {
    let env = TestEnv::new();
    // A small kept tail, so `/compact` has something to summarise.
    let mut layer: Value = serde_json::from_str(settings.unwrap_or("{}")).unwrap();
    layer["compaction"] = json!({"keepRecentTokens": 1});
    write_settings(&env, &layer.to_string());
    env.write("work/a.txt", "hello\n");
    let server = FakeServer::start(script(), opts).expect("fake");
    let engine: Arc<dyn Engine> = Arc::new(oxa_engine::HttpEngine::new(&server.url(), None).expect("engine"));
    let dirs = Dirs::under(env.root());
    let settings = Settings::load(&dirs, &env.work(), true);
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let mut s = Session::open(
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
            resources: Resources::default(),
            model: None,
            thinking: Some(level),
            mode: None,
            session_id: Some("t5".to_string()),
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .expect("open");
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    s.prompt("first".to_string(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt 1");
    s.prompt("second".to_string(), Vec::new(), &mut sink, &mut ui)
        .expect("prompt 2");
    s.compact(None, &mut sink, &mut ui).expect("compact");
    Run {
        reqs: server.requests(),
        events: sink.0,
        root: env.root().to_string_lossy().into_owned(),
    }
}

fn kwargs(r: &Recorded) -> Value {
    let body: Value = serde_json::from_slice(&r.body).expect("json body");
    body["chat_template_kwargs"].clone()
}

fn golden(thinking: &[bool]) -> Vec<Value> {
    thinking
        .iter()
        .map(|t| json!({"preserve_thinking": true, "enable_thinking": t}))
        .collect()
}

/// Every request reuses the whole previous prompt (the toggles change no sent byte); after the
/// compaction's rebase the whole frozen prefix is reused. The loop reported no cache miss and
/// the invariant checker is clean.
fn assert_cache_holds(r: &Run) {
    assert!(
        !r.events.iter().any(|e| matches!(e, AgentEvent::CacheMiss { .. })),
        "a cache miss was reported"
    );
    let mut rebases = Vec::new();
    for w in r.reqs.windows(2) {
        let cached = w[1].usage.expect("usage").cached;
        let expected = if w[0].kind == "compaction" {
            rebases.push((w[1].seq, rebase_why::COMPACTION));
            cache_bytes(&w[1].system, &w[1].tools_json, &[]).len() as u32
        } else {
            cache_bytes(&w[0].system, &w[0].tools_json, &w[0].messages).len() as u32
        };
        assert!(
            cached >= expected,
            "request {}: cached {cached} < expected {expected}",
            w[1].seq
        );
    }
    // The W rules; the hint rules are not this test's subject (a manual `/compact` runs in the
    // foreground).
    let broken: Vec<_> = check_all(&r.reqs, &rebases)
        .into_iter()
        .filter(|v| v.rule.starts_with('W'))
        .collect();
    assert_eq!(broken, Vec::new());
}

fn kinds(r: &Run) -> Vec<String> {
    r.reqs.iter().map(|q| q.kind.clone()).collect()
}

const KINDS: [&str; 6] = ["chat", "chat", "chat", "chat", "compaction", "prefill"];

#[test]
fn auto_thinks_on_the_first_reply_after_each_prompt_only() {
    // Unset policy under profile local = auto; an explicit auto under pi is the same.
    for settings in [
        r#"{"oxa": {"profile": "local"}}"#,
        r#"{"oxa": {"thinking": {"policy": "auto"}}}"#,
    ] {
        let r = run(Some(settings), ThinkingLevel::High, FakeOpts::default());
        assert_eq!(kinds(&r), KINDS);
        let got: Vec<Value> = r.reqs.iter().map(kwargs).collect();
        // prompt 1, tool hop, tool hop (final reply), prompt 2, compaction summary, prefill.
        assert_eq!(
            got,
            golden(&[true, false, false, true, false, false]),
            "{settings}"
        );
        assert_cache_holds(&r);
    }
}

#[test]
fn fixed_reproduces_todays_requests() {
    let today = run(
        Some(r#"{"oxa": {"profile": "pi"}}"#),
        ThinkingLevel::High,
        FakeOpts::default(),
    );
    assert_eq!(
        today.reqs.iter().map(kwargs).collect::<Vec<_>>(),
        golden(&[true, true, true, true, false, true])
    );
    for settings in [
        r#"{"oxa": {"profile": "pi"}}"#,
        r#"{"oxa": {"profile": "pi", "thinking": {"policy": "fixed"}}}"#,
    ] {
        let r = run(Some(settings), ThinkingLevel::High, FakeOpts::default());
        assert_eq!(
            today.bodies(),
            r.bodies(),
            "{settings}: request bodies byte for byte"
        );
        assert_cache_holds(&r);
    }
    // `fixed` wins under profile local too (the tools differ there; the thinking does not).
    let r = run(
        Some(r#"{"oxa": {"profile": "local", "thinking": {"policy": "fixed"}}}"#),
        ThinkingLevel::High,
        FakeOpts::default(),
    );
    assert_eq!(
        r.reqs.iter().map(kwargs).collect::<Vec<_>>(),
        golden(&[true, true, true, true, false, true])
    );
}

#[test]
fn level_off_always_wins() {
    let r = run(
        Some(r#"{"oxa": {"profile": "local"}}"#),
        ThinkingLevel::Off,
        FakeOpts::default(),
    );
    assert_eq!(r.reqs.iter().map(kwargs).collect::<Vec<_>>(), golden(&[false; 6]));
    assert_cache_holds(&r);
}

#[test]
fn auto_acts_as_fixed_where_a_toggle_is_not_cache_safe() {
    // The Flash engine keeps no reasoning and its template puts the reasoning-effort line at
    // the start of the system message when thinking is on: a toggle would re-prefill it all.
    let opts = FakeOpts {
        models: vec![FakeModel::flash("fake-model", 1 << 20, None)],
        ..FakeOpts::default()
    };
    let r = run(
        Some(r#"{"oxa": {"profile": "local"}}"#),
        ThinkingLevel::High,
        opts,
    );
    let got: Vec<bool> = r.reqs.iter().map(|q| q.thinking).collect();
    assert_eq!(got, vec![true, true, true, true, false, true]);
    // No preserve_thinking on Flash (F8): the kwargs carry the boolean alone.
    assert_eq!(kwargs(&r.reqs[0]), json!({"enable_thinking": true}));
}
