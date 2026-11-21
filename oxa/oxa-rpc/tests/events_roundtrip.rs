//! Desk Z10: a JSON / RPC event record reads back into the `AgentEvent` it was written from, so
//! a client of a remote `oxa --mode rpc` feeds the same reducer as a local session.
//! Isolation: `oxa_fake::TestEnv`.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{Fault, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{
    miss_cause, repair_cause, AgentEvent, CompactReason, CompactionResult, Entry, EntryBody, EventMessage,
    HeadlessUi, Level, MessageDelta, Mode, StopReason, ThinkingLevel, ToolCall, ToolOutput, Usage, WireMsg,
};
use oxa_rpc::{event_from_json, event_to_json, JsonSink};
use oxa_session::{MemStore, SessionStore};
use serde_json::{json, Value};
use std::sync::Arc;

const TS: u64 = 1_700_000_000_000;

fn usage() -> Usage {
    Usage {
        prompt: 120,
        completion: 7,
        cached: 100,
        ..Usage::default()
    }
}

fn plain(msg: WireMsg) -> EventMessage {
    EventMessage {
        msg,
        ts_ms: TS,
        stop: None,
        usage: None,
        model: None,
        tool_name: None,
        is_error: false,
        error_message: None,
    }
}

fn call() -> ToolCall {
    ToolCall {
        id: "call_1".to_string(),
        name: "read".to_string(),
        args_raw: r#"{"path":"src/main.rs"}"#.to_string(),
    }
}

fn assistant(stop: StopReason, error: Option<&str>) -> EventMessage {
    let mut msg = WireMsg::assistant("");
    msg.content = Some("I will read the file.".to_string());
    msg.reasoning = Some("The user wants the entry point.".to_string());
    msg.tool_calls = vec![call()];
    EventMessage {
        stop: Some(stop),
        usage: Some(usage()),
        model: Some("glm-5.3".to_string()),
        error_message: error.map(str::to_string),
        ..plain(msg)
    }
}

fn tool_result(is_error: bool) -> EventMessage {
    EventMessage {
        tool_name: Some("read".to_string()),
        is_error,
        ..plain(WireMsg::tool_result("call_1", "fn main() {}"))
    }
}

fn update(delta: MessageDelta) -> AgentEvent {
    AgentEvent::MessageUpdate {
        usage: usage(),
        delta,
    }
}

/// One or more events of every kind, with every optional field both set and unset.
fn every_kind() -> Vec<AgentEvent> {
    let s = str::to_string;
    vec![
        AgentEvent::AgentStart,
        AgentEvent::AgentEnd {
            messages: vec![
                plain(WireMsg::user("hi")),
                plain(WireMsg::system("a summary")),
                assistant(StopReason::ToolUse, None),
                tool_result(false),
            ],
            will_retry: true,
        },
        AgentEvent::AgentSettled { aborted: true },
        AgentEvent::TurnStart,
        AgentEvent::TurnEnd {
            message: assistant(StopReason::Stop, None),
            tool_results: vec![tool_result(false), tool_result(true)],
        },
        AgentEvent::MessageStart {
            message: plain(WireMsg::user("read the entry point")),
        },
        AgentEvent::MessageEnd {
            message: assistant(StopReason::Error, Some("the engine closed the stream")),
        },
        update(MessageDelta::ThinkingStart { index: 0 }),
        update(MessageDelta::ThinkingDelta {
            index: 0,
            delta: s("The user"),
        }),
        update(MessageDelta::ThinkingEnd {
            index: 0,
            content: s("The user wants the entry point."),
        }),
        update(MessageDelta::TextStart { index: 1 }),
        update(MessageDelta::TextDelta {
            index: 1,
            delta: s("I will"),
        }),
        update(MessageDelta::TextEnd {
            index: 1,
            content: s("I will read the file."),
        }),
        update(MessageDelta::ToolcallStart {
            index: 2,
            id: s("call_1"),
            name: s("read"),
        }),
        update(MessageDelta::ToolcallDelta {
            index: 2,
            delta: s("{\"path\""),
        }),
        update(MessageDelta::ToolcallEnd {
            index: 2,
            call: call(),
        }),
        AgentEvent::ToolExecutionStart {
            tool_call_id: s("call_1"),
            tool_name: s("bash"),
            args: json!({"command": "ls"}),
        },
        AgentEvent::ToolExecutionUpdate {
            tool_call_id: s("call_1"),
            tool_name: s("bash"),
            args: json!({"command": "ls"}),
            partial: s("Cargo.toml\n"),
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id: s("call_1"),
            tool_name: s("edit"),
            result: ToolOutput {
                ui: json!({"diff": "-a\n+b\n", "exitCode": 0}),
                ..ToolOutput::ok("edited src/main.rs")
            },
            duration_ms: Some(41),
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id: s("call_2"),
            tool_name: s("read"),
            result: ToolOutput::error("no such file"),
            duration_ms: None,
        },
        AgentEvent::CompactionStart {
            reason: CompactReason::Threshold,
        },
        AgentEvent::CompactionEnd {
            reason: CompactReason::Manual,
            result: Some(CompactionResult {
                summary: s("We fixed the parser."),
                first_kept_entry_id: s("0a1b2c3d"),
                tokens_before: 90_000,
                estimated_tokens_after: 4_000,
                usage: Some(usage()),
                details: json!({"readFiles": ["a.rs"]}),
            }),
            aborted: false,
            will_retry: false,
            error_message: None,
        },
        AgentEvent::CompactionEnd {
            reason: CompactReason::Overflow,
            result: None,
            aborted: true,
            will_retry: true,
            error_message: Some(s("the summary request failed")),
        },
        AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2_000,
            error_message: s("503"),
        },
        AgentEvent::AutoRetryEnd {
            success: false,
            attempt: 3,
            final_error: Some(s("503")),
        },
        AgentEvent::SummarizationRetryScheduled {
            attempt: 1,
            max_attempts: 2,
            delay_ms: 500,
            error_message: s("timeout"),
        },
        AgentEvent::SummarizationRetryAttemptStart {
            source: "compaction",
            reason: Some(CompactReason::Threshold),
        },
        AgentEvent::SummarizationRetryAttemptStart {
            source: "branchSummary",
            reason: None,
        },
        AgentEvent::SummarizationRetryFinished,
        AgentEvent::QueueUpdate {
            steering: vec![s("stop that")],
            follow_up: vec![s("then run the tests"), s("and commit")],
        },
        AgentEvent::EntryAppended {
            entry: Entry {
                id: s("0a1b2c3d"),
                parent: Some(s("00000001")),
                ts_ms: TS,
                body: EntryBody::Custom {
                    custom_type: s("my-extension.state"),
                    data: json!({"count": 2}),
                },
            },
        },
        AgentEvent::ModelSelect {
            model: s("glm-5.3"),
            previous: Some(s("deepseek-chat")),
        },
        AgentEvent::ThinkingLevelChanged {
            level: ThinkingLevel::High,
        },
        AgentEvent::SessionInfoChanged {
            name: Some(s("parser fix")),
        },
        AgentEvent::SessionInfoChanged { name: None },
        AgentEvent::BashExecutionUpdate {
            id: Some(s("b1")),
            delta: s("ok\n"),
        },
        AgentEvent::ExtensionError {
            extension_path: s("/home/u/.oxa/extensions/x.wasm"),
            event: s("tool_call"),
            error: s("trap"),
        },
        AgentEvent::Prefill {
            done: 10,
            total: 40,
            secs: 3,
        },
        AgentEvent::EngineQueue {
            pos: Some(2),
            wait_s: 15,
        },
        AgentEvent::EngineQueue { pos: None, wait_s: 2 },
        AgentEvent::PrefixEpoch {
            sha256: std::array::from_fn(|i| u8::try_from(i * 8 + 3).unwrap_or(0)),
            reason: s("tools changed"),
            reprefill_tokens: 12_000,
        },
        AgentEvent::CacheMiss {
            expected: 9_000,
            cached: 0,
            cause: miss_cause::ENGINE_EVICTED,
        },
        AgentEvent::ModeChanged { mode: Mode::Plan },
        AgentEvent::Notice {
            level: Level::Warning,
            text: s("a setting was ignored"),
        },
        AgentEvent::ToolRepair {
            attempt: 2,
            cause: repair_cause::BAD_JSON,
            total: 3,
            stopped: true,
        },
    ]
}

#[test]
fn every_event_kind_reads_back_as_the_event_that_was_written() {
    for event in every_kind() {
        let record = event_to_json(&event);
        let back = event_from_json(&record).unwrap_or_else(|| panic!("no event for {record}"));
        assert_eq!(back, event, "record {record}");
        assert_eq!(event_to_json(&back), record);
    }
}

/// What the record does not carry comes back in the record's own terms: the next encoding is
/// the same record, so a client that forwards events does not change them.
#[test]
fn a_record_narrower_than_its_event_encodes_the_same_again() {
    let mut text_only = WireMsg::assistant("");
    text_only.content = None;
    text_only.tool_calls = vec![
        // Streamed arguments that are not JSON, and a JSON string that itself holds JSON.
        ToolCall {
            id: "c1".to_string(),
            name: "bash".to_string(),
            args_raw: "{\"command\": \"ls".to_string(),
        },
        ToolCall {
            id: "c2".to_string(),
            name: "bash".to_string(),
            args_raw: r#""{\"command\":\"ls\"}""#.to_string(),
        },
        ToolCall {
            id: "c3".to_string(),
            name: "bash".to_string(),
            args_raw: "{ \"command\" :  \"ls\" }".to_string(),
        },
    ];
    let events = [
        // No stop reason, usage or model yet; usage the provider did not report.
        AgentEvent::MessageStart {
            message: plain(text_only),
        },
        AgentEvent::MessageUpdate {
            usage: Usage::unreported(),
            delta: MessageDelta::TextStart { index: 0 },
        },
        // A tool result without its call id or name, and empty display data.
        AgentEvent::MessageEnd {
            message: EventMessage {
                msg: WireMsg {
                    tool_call_id: None,
                    ..WireMsg::tool_result("", "done")
                },
                ..plain(WireMsg::user(""))
            },
        },
        AgentEvent::ToolExecutionEnd {
            tool_call_id: "c1".to_string(),
            tool_name: "bash".to_string(),
            result: ToolOutput {
                ui: json!({}),
                ..ToolOutput::ok("done")
            },
            duration_ms: None,
        },
    ];
    for event in events {
        let record = event_to_json(&event);
        let back = event_from_json(&record).unwrap_or_else(|| panic!("no event for {record}"));
        assert_eq!(event_to_json(&back), record);
    }
}

#[test]
fn a_record_that_is_not_a_known_event_is_none() {
    for record in [
        json!({"type": "a_future_event", "x": 1}),
        json!({"type": "response", "command": "get_state", "success": true}),
        json!({"type": "extension_ui_request", "id": "1", "method": "confirm"}),
        json!({"type": "session", "version": 3, "id": "s", "cwd": "/"}),
        json!({"no": "type"}),
        json!("agent_start"),
        Value::Null,
        // Known types whose fields do not fit.
        json!({"type": "agent_settled"}),
        json!({"type": "agent_settled", "aborted": "yes"}),
        json!({"type": "prefill", "done": -1, "total": 4, "secs": 0}),
        json!({"type": "prefix_epoch", "sha256": "abc", "reason": "r", "reprefillTokens": 1}),
        json!({"type": "mode_changed", "mode": "a_future_mode"}),
        json!({"type": "message_start", "message": {"role": "narrator", "content": "x", "timestamp": 1}}),
        json!({"type": "entry_appended", "entry": {"type": "a_future_entry"}}),
        json!({"type": "tool_repair", "attempt": 1, "cause": "a_future_cause", "total": 1, "stopped": false}),
    ] {
        assert_eq!(event_from_json(&record), None, "record {record}");
    }
}

/// A newer peer may add fields and name a new cache-miss cause; the event still arrives.
#[test]
fn additions_from_a_newer_peer_do_not_drop_the_event() {
    assert_eq!(
        event_from_json(&json!({"type": "agent_settled", "aborted": false, "runId": "r1"})),
        Some(AgentEvent::AgentSettled { aborted: false })
    );
    assert_eq!(
        event_from_json(
            &json!({"type": "cache_miss", "expected": 10, "cached": 2, "cause": "a_future_cause"})
        ),
        Some(AgentEvent::CacheMiss {
            expected: 10,
            cached: 2,
            cause: miss_cause::UNKNOWN,
        })
    );
}

/// The records of a real turn, as a `--mode json` client receives them, all read back.
#[test]
fn the_records_of_a_real_turn_read_back() {
    let env = TestEnv::new();
    let mut turn = ScriptTurn::text("the reply");
    turn.fault = Some(Fault::Queued { count: 1, pos: true });
    let store: Arc<dyn SessionStore> = Arc::new(MemStore::new());
    let mut session = Session::open(
        Deps {
            engine: Arc::new(ScriptedEngine::new(vec![turn])),
            store,
            tools: Vec::new(),
            hooks: Vec::new(),
            policy: Box::new(policy::Deny),
            sandbox: Arc::new(sandbox::NoBwrap),
            settings: Settings::default(),
            dirs: Dirs::under(env.root()),
            cwd: env.work(),
            selection: ToolSelection::default(),
            resources: Default::default(),
            model: None,
            thinking: None,
            mode: None,
            session_id: None,
            jobs: oxa_core::fresh_jobs(),
        },
        Open::New,
    )
    .expect("open");
    let mut sink = JsonSink {
        out: Vec::new(),
        error: None,
    };
    session
        .prompt(
            "hi".to_string(),
            Vec::new(),
            &mut sink,
            &mut HeadlessUi::default(),
        )
        .expect("prompt");
    let text = String::from_utf8(sink.out).expect("utf-8 records");
    let mut kinds = Vec::new();
    for line in text.lines() {
        let record: Value = serde_json::from_str(line).expect("a JSON record");
        let event = event_from_json(&record).unwrap_or_else(|| panic!("no event for {record}"));
        assert_eq!(event_to_json(&event), record);
        kinds.push(event.name());
    }
    for kind in ["agent_start", "engine_queue", "message_end", "agent_end"] {
        assert!(kinds.contains(&kind), "the turn has no {kind} record: {kinds:?}");
    }
}

/// Stand-ins for the policy and sandbox crates, which `oxa-rpc` does not depend on: this
/// session runs no tools.
mod policy {
    use oxa_proto::{Access, Decision, Mode, Policy};
    pub struct Deny;
    impl Policy for Deny {
        fn decide(&self, _mode: Mode, _a: &Access, _cwd: &std::path::Path) -> Decision {
            Decision::Deny("no tools in this test")
        }
        fn remember(&mut self, _a: &Access) {}
    }
}

mod sandbox {
    use oxa_proto::{codes, CancelToken, ExecResult, ExecSpec, OxaError, Sandbox};
    pub struct NoBwrap;
    impl Sandbox for NoBwrap {
        fn check(&self) -> Result<(), OxaError> {
            Err(OxaError::new(codes::NO_SANDBOX, "no sandbox in this test"))
        }
        fn bwrap_args(&self, _s: &ExecSpec) -> Vec<String> {
            Vec::new()
        }
        fn run(&self, _s: &ExecSpec, _c: &CancelToken, _live: &mut dyn FnMut(&[u8])) -> ExecResult {
            ExecResult::default()
        }
    }
}
