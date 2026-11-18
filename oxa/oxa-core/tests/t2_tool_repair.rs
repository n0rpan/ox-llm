//! T2 (wave H): the append-only repair turn for malformed tool calls.
//! Content shapes follow the engine's lenient final parse (`chat/src/parser.rs`): a call it
//! cannot parse comes back as content, glued to the prose before it.
//! Goldens: `tests/golden/t2_*.txt` (request hashes; `OXA_UPDATE_GOLDEN=1` rewrites them).

use oxa_compact::CompactPolicy;
use oxa_config::{Dirs, QueueMode, Settings};
use oxa_core::repair::{classify, find_opener, Cause, Declared, CUSTOM_TYPE, MAX_IN_ROW};
use oxa_core::{append_entry, prompt, run_loop, Queue, RunEnd, SessionState, Shared, Stats, Toolset};
use oxa_fake::{Recorded, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_perm::ModePolicy;
use oxa_proto::{
    repair_cause, shown_why, AgentEvent, CancelToken, Engine, EntryBody, Finish, FrozenPrefix, HeadlessUi,
    Mode, Policy, ThinkingLevel, Tool, Value, VecSink, WireMsg,
};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::{MemStore, SessionStore, Transcript, Tree};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn tools() -> Vec<Arc<dyn Tool>> {
    let mut t = oxa_tools::builtin_tools();
    t.sort_by(|a, b| a.spec().name.cmp(&b.spec().name));
    t
}

fn state(env: &TestEnv, turns: Vec<ScriptTurn>, settings: Settings) -> (SessionState, Arc<ScriptedEngine>) {
    let eng = Arc::new(ScriptedEngine::new(turns));
    let engine: Arc<dyn Engine> = eng.clone();
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let cwd = env.work();
    std::fs::write(cwd.join("a.txt"), "alpha\n").unwrap();
    let sref = store.create(&cwd, None, None).unwrap();
    let info = engine.info(None).unwrap();
    // A fixed prefix without the cwd: the request hashes do not depend on the temp path.
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
        wake: Default::default(),
    });
    let tools = tools();
    let st = SessionState {
        goal: None,
        output_schema: None,
        engine,
        store,
        hooks: Vec::new(),
        policy: Box::new(ModePolicy::new()) as Box<dyn Policy>,
        sandbox: Arc::new(NoSandbox),
        settings,
        dirs: Dirs::under(env.root()),
        cwd: cwd.clone(),
        all_tools: tools.clone(),
        selection: Default::default(),
        active: Toolset {
            direct: tools,
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

fn run(st: &mut SessionState, prompt_text: &str) -> (RunEnd, Vec<AgentEvent>) {
    append_entry(
        st,
        EntryBody::Msg {
            wire: WireMsg::user(prompt_text),
            parts: Vec::new(),
            tool: None,
        },
    )
    .unwrap();
    let mut sink = VecSink::default();
    let mut ui = HeadlessUi::default();
    let end = run_loop(st, &mut sink, &mut ui).unwrap();
    (end, sink.0)
}

fn call(id: &str, name: &str, args: &str) -> ScriptTurn {
    ScriptTurn::tool(id, name, args)
}

fn golden(name: &str, reqs: &[Recorded]) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/golden/t2_{name}.txt"));
    let update = std::env::var("OXA_UPDATE_GOLDEN").is_ok_and(|v| v == "1");
    oxa_fake::golden::compare_golden(&path, &oxa_fake::golden::request_hashes(reqs), update).unwrap();
}

fn repairs(events: &[AgentEvent]) -> Vec<(u32, &'static str, u32, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolRepair {
                attempt,
                cause,
                total,
                stopped,
            } => Some((*attempt, *cause, *total, *stopped)),
            _ => None,
        })
        .collect()
}

const UNKNOWN_FUNCTION: &str =
    "I'll look at it.<tool_call>\n<function=cat>\n<parameter=path>\na.txt\n</parameter>\n</function>\n</tool_call>";
const UNKNOWN_PARAMETER: &str =
    "<tool_call>\n<function=read>\n<parameter=file>\na.txt\n</parameter>\n</function>\n</tool_call>";
const MISSING_REQUIRED: &str =
    "<tool_call>\n<function=read>\n<parameter=offset>\n2\n</parameter>\n</function>\n</tool_call>";
const UNCLOSED: &str = "Reading.<tool_call>\n<function=read>\n<parameter=path>\na.txt\n</parameter>\n";
const BAD_JSON: &str = "<tool_call>\n<function=read>\n<parameter=path>\na.txt\n</parameter>\n<parameter=limit>\nten\n</parameter>\n</function>\n</tool_call>";
const WRONG_TYPE: &str = "<tool_call>\n<function=read>\n<parameter=path>\na.txt\n</parameter>\n<parameter=limit>\n[1, 2]\n</parameter>\n</function>\n</tool_call>";

/// The six causes: one repair, then the model writes a valid call and finishes.
fn one_case(name: &str, malformed: &str, cause: &'static str, line: String) {
    let env = TestEnv::new();
    let turns = vec![
        ScriptTurn::text(malformed),
        call("c1", "read", r#"{"path":"a.txt"}"#),
        ScriptTurn::text("done"),
    ];
    let (mut st, eng) = state(&env, turns, Settings::default());
    let (end, events) = run(&mut st, "show a.txt");
    assert_eq!(end, RunEnd::Stopped);
    let reqs = eng.requests();
    assert_eq!(reqs.len(), 3);
    // The second request = the first + the malformed reply exactly as streamed (W2) + the
    // fixed repair message (an append, W1).
    let second = &reqs[1].messages;
    assert_eq!(second.len(), 3);
    assert_eq!(second[1], WireMsg::assistant(malformed));
    let text = prompt::repair_message(&line);
    assert_eq!(second[2], WireMsg::user(text.clone()));
    assert!(oxa_fake::check_w1(&reqs, &[]).is_empty());
    assert_eq!(repairs(&events), vec![(1, cause, 1, false)]);
    assert_eq!(st.stats.tool_repairs, 1);
    let stored: Vec<_> = st
        .tree
        .branch(st.tree.leaf())
        .into_iter()
        .filter_map(|e| match &e.body {
            EntryBody::CustomMessage {
                custom_type,
                details,
                display,
                content,
            } if custom_type == CUSTOM_TYPE => Some((details.clone(), *display, content.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        stored,
        vec![(serde_json::json!({"cause": cause, "attempt": 1}), false, text)]
    );
    golden(name, &reqs);
}

#[test]
fn unknown_function() {
    one_case(
        "unknown_function",
        UNKNOWN_FUNCTION,
        repair_cause::UNKNOWN_FUNCTION,
        prompt::repair_unknown_function("cat", &["bash", "edit", "find", "grep", "ls", "read", "write"]),
    );
}

#[test]
fn unknown_parameter_names_the_valid_ones() {
    one_case(
        "unknown_parameter",
        UNKNOWN_PARAMETER,
        repair_cause::UNKNOWN_PARAMETER,
        prompt::repair_unknown_parameter("read", "file", &["path", "offset", "limit"]),
    );
}

#[test]
fn missing_required() {
    one_case(
        "missing_required",
        MISSING_REQUIRED,
        repair_cause::MISSING_REQUIRED,
        prompt::repair_missing_required("read", &["path"]),
    );
}

#[test]
fn unclosed_block() {
    one_case(
        "unclosed",
        UNCLOSED,
        repair_cause::UNCLOSED,
        prompt::repair_unclosed("</function>"),
    );
}

#[test]
fn bad_json() {
    one_case(
        "bad_json",
        BAD_JSON,
        repair_cause::BAD_JSON,
        prompt::repair_bad_json("read", "limit"),
    );
}

#[test]
fn wrong_type() {
    one_case(
        "wrong_type",
        WRONG_TYPE,
        repair_cause::WRONG_TYPE,
        prompt::repair_wrong_type("read", "limit", "integer"),
    );
}

#[test]
fn cap_of_two_then_stop_with_the_raw_text_shown() {
    let env = TestEnv::new();
    let turns = vec![
        ScriptTurn::text(UNKNOWN_PARAMETER),
        ScriptTurn::text(UNKNOWN_PARAMETER),
        ScriptTurn::text(UNKNOWN_PARAMETER),
        ScriptTurn::text("never requested"),
    ];
    let (mut st, eng) = state(&env, turns, Settings::default());
    let (end, events) = run(&mut st, "go");
    assert_eq!(end, RunEnd::Stopped);
    let reqs = eng.requests();
    assert_eq!(reqs.len(), 3, "two repairs, then no fourth request");
    assert_eq!(MAX_IN_ROW, 2);
    let r = repairs(&events);
    assert_eq!(
        r,
        vec![
            (1, repair_cause::UNKNOWN_PARAMETER, 1, false),
            (2, repair_cause::UNKNOWN_PARAMETER, 2, false),
            (2, repair_cause::UNKNOWN_PARAMETER, 2, true),
        ]
    );
    assert_eq!(st.stats.tool_repairs, 2);
    // The last reply stays on the wire as streamed (the raw text the user sees), followed by
    // the shown stop message (R6 shape), which is never sent.
    let branch: Vec<_> = st.tree.branch(st.tree.leaf()).into_iter().cloned().collect();
    let last_msg = branch
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            EntryBody::Msg { wire, .. } => Some(wire.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(last_msg, WireMsg::assistant(UNKNOWN_PARAMETER));
    let EntryBody::Shown { text, why } = &branch.last().unwrap().body else {
        panic!("the run ends with a shown stop message");
    };
    assert_eq!(*why, shown_why::NOTICE);
    assert_eq!(
        text,
        "Run stopped: the model wrote a malformed tool call 3 times in a row: unknown parameter (Tool \"read\" has no parameter \"file\". Valid parameters: path, offset, limit.)\nFix: send the request again, rephrase it, or switch model with /model"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Notice { text: t, .. } if t == text)));
    golden("cap", &reqs);
}

#[test]
fn a_valid_call_resets_the_row() {
    let env = TestEnv::new();
    let turns = vec![
        ScriptTurn::text(MISSING_REQUIRED),
        call("c1", "read", r#"{"path":"a.txt"}"#),
        ScriptTurn::text(MISSING_REQUIRED),
        ScriptTurn::text(MISSING_REQUIRED),
        ScriptTurn::text("ok"),
    ];
    let (mut st, eng) = state(&env, turns, Settings::default());
    let (end, events) = run(&mut st, "go");
    assert_eq!(end, RunEnd::Stopped);
    assert_eq!(eng.requests().len(), 5);
    let attempts: Vec<u32> = repairs(&events).iter().map(|r| r.0).collect();
    assert_eq!(attempts, vec![1, 1, 2]);
    assert_eq!(st.stats.tool_repairs, 3);
}

#[test]
fn quoted_calls_after_real_calls_and_in_code_are_not_repaired() {
    let env = TestEnv::new();
    let mut with_calls = call("c1", "read", r#"{"path":"a.txt"}"#);
    // A reply with real calls whose content quotes the format is never a candidate.
    with_calls.text = Some("<tool_call>\n<function=read>\n".to_string());
    let turns = vec![
        with_calls,
        ScriptTurn::text(
            "Calls look like `<tool_call>` blocks:\n```\n<tool_call>\n<function=read>\n```\nDone.",
        ),
    ];
    let (mut st, eng) = state(&env, turns, Settings::default());
    let (end, events) = run(&mut st, "explain");
    assert_eq!(end, RunEnd::Stopped);
    assert_eq!(eng.requests().len(), 2);
    assert!(repairs(&events).is_empty());
    assert_eq!(st.stats.tool_repairs, 0);
}

#[test]
fn length_cut_recovers_without_tool_repair_and_setting_off_still_stops() {
    let env = TestEnv::new();
    let mut cut = ScriptTurn::text(UNCLOSED);
    cut.finish = Finish::Length;
    let (mut st, eng) = state(&env, vec![cut, ScriptTurn::text("done")], Settings::default());
    let (end, events) = run(&mut st, "go");
    assert_eq!(end, RunEnd::Stopped);
    assert_eq!(eng.requests().len(), 3, "chat, prefill, length continuation");
    assert!(repairs(&events).is_empty());
    assert_eq!(st.stats.tool_repairs, 0);

    let off = Settings::from_values(serde_json::json!({"oxa": {"toolRepair": false}}), Value::Null);
    let (mut st, eng) = state(&env, vec![ScriptTurn::text(UNKNOWN_PARAMETER)], off);
    let (end, events) = run(&mut st, "go");
    assert_eq!(end, RunEnd::Stopped);
    assert_eq!(eng.requests().len(), 1);
    assert!(repairs(&events).is_empty());
}

#[test]
fn detection_rule_edges() {
    // Openers: `<tool_call>` anywhere outside code; `<function=` only before a name char.
    assert_eq!(find_opener("x<tool_call>"), Some(1));
    assert_eq!(find_opener("x <function=read>"), Some(2));
    assert_eq!(find_opener("x <function=>"), None);
    assert_eq!(find_opener("x <function= read>"), None);
    // Inline spans of the same run length, and fences of ``` or ~~~.
    assert_eq!(find_opener("a `<tool_call>` b"), None);
    assert_eq!(find_opener("a ``x`<tool_call>`` b"), None);
    assert_eq!(find_opener("a `x` <tool_call>"), Some(6));
    assert_eq!(find_opener("~~~\n<tool_call>\n~~~"), None);
    assert_eq!(
        find_opener("  ```rust\n<tool_call>\n  ```\n<tool_call>"),
        Some(28)
    );
    // An unclosed inline span ends at the end of its line.
    assert_eq!(find_opener("a `b\n<tool_call>"), Some(5));
}

#[test]
fn classify_edges() {
    let schema = r#"{"type":"object","properties":{"path":{"type":"string"},"n":{"type":"integer"},"f":{"type":"boolean"}},"required":["path"]}"#;
    let tools = [Declared {
        name: "t",
        schema_json: schema,
    }];
    let c = |s: &str| classify(s, &tools);
    assert_eq!(c("<tool_call>"), Cause::Unclosed { tag: "</tool_call>" });
    assert_eq!(c("<tool_call>\n{\"name\":\"t\"}"), Cause::Format);
    assert_eq!(c("<function=t"), Cause::Unclosed { tag: ">" });
    assert_eq!(
        c("<function=t>\n<parameter=path>\nx\n</parameter>\n</function>\n"),
        Cause::Format,
        "a bare function block (no wrapper) that is complete is a format problem"
    );
    assert_eq!(
        c("<tool_call>\n<function=t>\n<parameter=path>\nx\n</parameter>\n</function>\n"),
        Cause::Unclosed { tag: "</tool_call>" }
    );
    // Engine coercions are accepted: "5", 1.0 for an integer, True for a boolean.
    for ok in ["\"5\"", "1.0", "True"] {
        let param = if ok == "True" { "f" } else { "n" };
        let block = format!("<tool_call>\n<function=t>\n<parameter=path>\nx\n</parameter>\n<parameter={param}>\n{ok}\n</parameter>\n</function>\n</tool_call>");
        assert_eq!(c(&block), Cause::Format, "{ok}");
    }
    let block = "<tool_call>\n<function=t>\n<parameter=path>\nx\n</parameter>\n<parameter=n>\n1.5\n</parameter>\n</function>\n</tool_call>";
    assert_eq!(
        c(block),
        Cause::WrongType {
            tool: "t".into(),
            param: "n".into(),
            expected: "integer".into()
        }
    );
    let block = "<tool_call>\n<function=t>\n<parameter=path>\nx\n";
    assert_eq!(c(block), Cause::Unclosed { tag: "</parameter>" });
}

#[test]
fn bash_job_only_call_is_not_missing_required() {
    // Issue #127 (b): `command` is no longer required in the bash schema (a `job` read has no
    // command). The repair classifier reads the schema's `required` set, so a job-only call
    // must not be flagged as `MissingRequired` before it reaches `BashTool::parse`.
    let tools = [Declared {
        name: "bash",
        schema_json: oxa_tools::descriptions::BASH.schema_json,
    }];
    let block =
        "<tool_call>\n<function=bash>\n<parameter=job>\njob-1\n</parameter>\n</function>\n</tool_call>";
    assert_eq!(classify(block, &tools), Cause::Format);
    // A call with neither `command` nor `job` is still a format problem here (the tool's own
    // `parse` reports the exact-one error), not a schema-level missing-required.
    let block = "<tool_call>\n<function=bash>\n</function>\n</tool_call>";
    assert_eq!(classify(block, &tools), Cause::Format);
}

#[test]
fn repair_message_is_fixed_text() {
    assert_eq!(
        prompt::repair_message(&prompt::repair_missing_required("read", &["path"])),
        "Your previous reply contains a tool call that could not be parsed, so no tool was run. Tool \"read\" is missing the required parameter(s): path. Write the tool call again, complete and in the tool call format."
    );
}

#[test]
fn output_schema_one_repair_budget_across_tools_and_prompts() {
    use oxa_core::output_schema::OutputSchema;
    use serde_json::json;
    let env = TestEnv::new();
    let good = r#"{"answer":42}"#;
    for (turns, count, failed) in [
        (vec![ScriptTurn::text(good)], 1, false),
        (vec![ScriptTurn::text("bad"), ScriptTurn::text(good)], 2, false),
        (vec![ScriptTurn::text("bad"), ScriptTurn::text("bad")], 2, true),
        (
            vec![call("c1", "read", r#"{"path":"a.txt"}"#), ScriptTurn::text(good)],
            2,
            false,
        ),
        (
            vec![
                ScriptTurn::text("bad"),
                call("c1", "read", r#"{"path":"a.txt"}"#),
                ScriptTurn::text("bad"),
            ],
            3,
            true,
        ),
    ] {
        let (mut st, eng) = state(&env, turns, Settings::default());
        st.output_schema = Some(
            OutputSchema::new(json!({"type":"object", "required":["answer"],
            "properties":{"answer":{"type":"number"}}}))
            .unwrap(),
        );
        let prefix = st.prefix.clone();
        append_entry(
            &mut st,
            EntryBody::Msg {
                wire: WireMsg::user("answer"),
                parts: vec![],
                tool: None,
            },
        )
        .unwrap();
        let result = run_loop(&mut st, &mut VecSink::default(), &mut HeadlessUi::default());
        if failed {
            assert_eq!(result.unwrap_err().code, oxa_proto::codes::SCHEMA_MISMATCH);
        } else {
            assert_eq!(result.unwrap(), RunEnd::Stopped);
        }
        assert_eq!(eng.requests().len(), count);
        assert_eq!(st.prefix, prefix, "schema must not change the frozen prefix");
        let repairs = st
            .tree
            .branch(st.tree.leaf())
            .iter()
            .filter(|e| {
                matches!(
                    &e.body, EntryBody::CustomMessage {custom_type, ..} if custom_type == "oxa.schema_repair"
                )
            })
            .count();
        assert!(repairs <= 1);
        // A new user prompt gets a fresh repair budget, not a persisted lifetime counter.
        eng.push(ScriptTurn::text("bad"));
        eng.push(ScriptTurn::text(good));
        let (end, _) = run(&mut st, "again");
        assert_eq!(end, RunEnd::Stopped);
        assert_eq!(eng.requests().len(), count + 2);
    }
    let (mut normal, eng) = state(
        &env,
        vec![ScriptTurn::text("ordinary prose")],
        Settings::default(),
    );
    assert_eq!(run(&mut normal, "normal").0, RunEnd::Stopped);
    assert_eq!(eng.requests().len(), 1);
    assert_eq!(
        eng.requests()[0].messages.len(),
        1,
        "no schema prompt in normal work"
    );
}

#[test]
fn output_schema_instructions_do_not_survive_resume_without_flag() {
    let env = TestEnv::new();
    let (mut st, eng) = state(
        &env,
        vec![ScriptTurn::text("bad"), ScriptTurn::text("42")],
        Settings::default(),
    );
    st.output_schema =
        Some(oxa_core::output_schema::OutputSchema::new(serde_json::json!({"type":"number"})).unwrap());
    assert_eq!(run(&mut st, "answer").0, RunEnd::Stopped);
    let requests = eng.requests();
    assert!(requests[0].messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("JSON matching this schema"))));
    assert!(requests[1].messages.iter().any(|m| m
        .content
        .as_deref()
        .is_some_and(|s| s.contains("did not match the output schema"))));

    // Also cover contracts written by earlier versions, not just new sessions.
    for custom_type in ["oxa.output_schema", "oxa.schema_repair", "extension.note"] {
        append_entry(
            &mut st,
            EntryBody::CustomMessage {
                custom_type: custom_type.into(),
                content: custom_type.into(),
                display: false,
                details: Value::Null,
            },
        )
        .unwrap();
    }
    let branch: Vec<_> = st.tree.branch(st.tree.leaf()).into_iter().cloned().collect();
    st.transcript = Transcript::project(&branch);
    st.output_schema = None;
    eng.push(ScriptTurn::text("ordinary prose"));
    assert_eq!(run(&mut st, "resume normally").0, RunEnd::Stopped);
    let requests = eng.requests();
    let resumed = &requests.last().unwrap().messages;
    assert!(resumed
        .iter()
        .any(|m| m.content.as_deref() == Some("extension.note")));
    assert!(
        !resumed
            .iter()
            .any(|m| m.content.as_deref().is_some_and(|s| { s.contains("schema") })),
        "process-local schema instructions must not be restored"
    );
}

#[test]
fn output_schema_upstream_requires_both_cloud_and_advertised_capability() {
    struct Probe {
        inner: Arc<ScriptedEngine>,
        schemas: Mutex<Vec<Option<Value>>>,
    }
    impl Engine for Probe {
        fn info(&self, target: Option<&str>) -> Result<oxa_proto::EngineInfo, oxa_proto::OxaError> {
            self.inner.info(target)
        }
        fn count_tokens(&self, text: &str) -> Result<u32, oxa_proto::OxaError> {
            self.inner.count_tokens(text)
        }
        fn chat(
            &self,
            r: &oxa_proto::ChatRequest,
            cancel: &CancelToken,
            on: &mut dyn FnMut(oxa_proto::StreamEvent),
        ) -> Result<oxa_proto::AssistantTurn, oxa_proto::OxaError> {
            self.schemas.lock().unwrap().push(r.output_schema.cloned());
            self.inner.chat(r, cancel, on)
        }
    }
    let env = TestEnv::new();
    for (remote, advertised) in [(false, false), (false, true), (true, false), (true, true)] {
        let (mut st, eng) = state(&env, vec![ScriptTurn::text("42")], Settings::default());
        let probe = Arc::new(Probe {
            inner: eng,
            schemas: Mutex::new(Vec::new()),
        });
        st.engine = probe.clone();
        st.info.caps.remote = remote;
        st.info.caps.response_format = advertised;
        st.output_schema =
            Some(oxa_core::output_schema::OutputSchema::new(serde_json::json!({"type":"number"})).unwrap());
        assert_eq!(run(&mut st, "answer").0, RunEnd::Stopped);
        assert_eq!(
            *probe.schemas.lock().unwrap(),
            vec![if remote && advertised {
                Some(serde_json::json!({"type":"number"}))
            } else {
                None
            }]
        );
    }
}
