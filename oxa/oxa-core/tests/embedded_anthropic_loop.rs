#![cfg(feature = "in-process-gateway")]
//! Real Session/ReadTool -> Engine bridge -> shared gateway -> owned native peer.
#[allow(dead_code)]
#[path = "../../oxc-gateway/tests/common/anthropic.rs"]
mod anthropic;
#[path = "../../oxc-gateway/tests/common/skeleton.rs"]
mod skeleton;

use anthropic::{MockAnthropic, Reply};
use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_engine::{EmbeddedEngine, Gateway, GatewayOptions, GatewayRuntime};
use oxa_fake::TestEnv;
use oxa_perm::ModePolicy;
use oxa_proto::{AgentEvent, HeadlessUi, Mode, Role, VecSink};
use oxa_resources::Resources;
use oxa_sandbox::NoSandbox;
use oxa_session::MemStore;
use oxa_tools::ReadTool;
use serde_json::{json, Value};
use std::{fs, sync::Arc};

#[test]
fn a_real_read_tool_loop_keeps_signed_state_and_settles_each_provider_request() {
    let env = TestEnv::new();
    env.write("work/λ", "synthetic file contents\n");
    // Reuse the recorded Python translator's native thinking/tool events. The
    // second unrelated tool is removed so this test executes only ReadTool.
    let case = include_str!("../../oxc-gateway/tests/fixtures/anthropic_streams.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|case| case["name"] == "thinking_tools")
        .unwrap();
    let events = case["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["index"].as_u64() != Some(3))
        .cloned()
        .collect();
    let expected_native = &case["native_blocks"].as_array().unwrap()[..3];
    let mock = MockAnthropic::start(vec![
        Reply::events(events),
        Reply::text("Synthetic file read.", 11, 7, 3, 5),
    ]);
    let mut fixture = skeleton::Fixture::new(&mock);
    fixture.config["cloud"]["models"][skeleton::MODEL]["context_window"] = json!(64000);
    fixture.config["cloud"]["models"][skeleton::MODEL]["max_output"] = json!(4096);
    let runtime = GatewayRuntime::new(
        fixture.config.clone(),
        fixture.key_path.clone(),
        fixture.ledger_dir.clone(),
        GatewayOptions::default(),
    )
    .unwrap();
    let engine = EmbeddedEngine::from_gateway(Gateway::new(runtime).unwrap()).unwrap();
    let deps = Deps {
        engine: Arc::new(engine),
        store: Arc::new(MemStore::new()),
        tools: vec![Arc::new(ReadTool::new())],
        hooks: vec![],
        policy: Box::new(ModePolicy::new()),
        sandbox: Arc::new(NoSandbox),
        settings: Settings::from_values(json!({"oxa":{"envNote":false}}), json!({})),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Resources::default(),
        model: Some(skeleton::MODEL.into()),
        thinking: None,
        mode: Some(Mode::Workspace),
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    let mut session = Session::open(deps, Open::New).unwrap();
    let (mut sink, mut ui) = (VecSink::default(), HeadlessUi::default());
    session
        .prompt("Read the synthetic file λ".into(), vec![], &mut sink, &mut ui)
        .unwrap();
    let wire = session.inner().transcript.wire();
    assert_eq!(
        wire.iter().map(|message| message.role).collect::<Vec<_>>(),
        vec![Role::User, Role::Assistant, Role::Tool, Role::Assistant]
    );
    assert_eq!(wire[1].tool_calls[0].id, "call_1");
    assert!(wire[2]
        .content
        .as_deref()
        .unwrap()
        .contains("synthetic file contents"));
    assert_eq!(wire[3].content.as_deref(), Some("Synthetic file read."));
    assert!(session.state().model_remote);
    assert_eq!(session.state().model_provider, Some("anthropic"));
    assert!(sink
        .0
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolExecutionEnd { .. })));
    let requests = mock.wait_for_requests(2);
    assert_eq!(
        requests.len(),
        2,
        "no prefill, HTTP daemon request or extra provider attempt"
    );
    assert_eq!(requests[1].body["messages"][1]["content"], json!(expected_native));
    assert!(
        requests[1].body["messages"][2]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("synthetic file contents")
    );
    let rows: Vec<Value> = fs::read_dir(&fixture.ledger_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("requests-")
        })
        .flat_map(|path| {
            fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|row| row["status"] == 200 && row["origin"] == "organic" && row["remote"] == true));
    let ledger = serde_json::to_string(&rows).unwrap();
    for private in [
        "synthetic file contents",
        "synthetic-signature",
        "synthetic-opaque-state",
    ] {
        assert!(!ledger.contains(private));
    }
    mock.assert_healthy();
}
