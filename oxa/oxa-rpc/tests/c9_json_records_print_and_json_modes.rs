//! C9 acceptance tests: JSON records, print and json modes. Owner: task C9.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_config::{Dirs, Settings};
use oxa_core::{Deps, Open, Session, ToolSelection};
use oxa_fake::{Fault, ScriptTurn, ScriptedEngine, TestEnv};
use oxa_proto::{
    codes, Access, AgentEvent, CancelToken, CompactReason, CompactionResult, Decision, Engine, EventMessage,
    ExecResult, ExecSpec, Level, MessageDelta, Mode, OxaError, Part, Policy, Sandbox, StopReason,
    ThinkingLevel, ToolCall, ToolOutput, Usage, WireMsg,
};
use oxa_proto::{Entry, EntryBody, EventSink};
use oxa_rpc::{
    entry_to_json, event_to_json, message_to_json, run_json, run_print, session_header_record, state_to_json,
    write_record, JsonSink,
};
use oxa_session::{encode_entry, FsStore, SessionHeader, SessionStore};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

/// A policy for sessions without tools: every access is refused.
struct DenyAll;

impl Policy for DenyAll {
    fn decide(&self, _mode: Mode, _a: &Access, _cwd: &Path) -> Decision {
        Decision::Deny("no tools in this test")
    }
    fn remember(&mut self, _a: &Access) {}
}

/// A sandbox that is never reached: the sessions here run no tools.
struct NoBwrap;

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

/// A new session over `engine`, stored under the test's own agent directory.
fn open(env: &TestEnv, engine: &Arc<ScriptedEngine>) -> Session {
    let store: Arc<dyn SessionStore> = Arc::new(FsStore::new(env.agent_dir().join("sessions")));
    let engine_dyn: Arc<dyn Engine> = engine.clone();
    let deps = Deps {
        engine: engine_dyn,
        store,
        tools: Vec::new(),
        hooks: Vec::new(),
        policy: Box::new(DenyAll),
        sandbox: Arc::new(NoBwrap),
        // Exact prompt-byte fixtures select note-free queues/RPC/replay.
        settings: Settings::from_values(
            serde_json::json!({"oxa": {"envNote": false}}),
            serde_json::json!({}),
        ),
        dirs: Dirs::under(env.root()),
        cwd: env.work(),
        selection: ToolSelection::default(),
        resources: Default::default(),
        model: None,
        thinking: None,
        mode: None,
        session_id: None,
        jobs: oxa_core::fresh_jobs(),
    };
    Session::open(deps, Open::New).expect("the session opens over the scripted engine")
}

fn prompt(text: &str) -> Vec<(String, Vec<Part>)> {
    vec![(text.to_string(), Vec::new())]
}

/// Every LF-terminated line of `out`, parsed as one JSON record.
fn records(out: &[u8]) -> Vec<Value> {
    out.split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("each line is one JSON record"))
        .collect()
}

fn user_msg(text: &str, ts: u64) -> EventMessage {
    EventMessage {
        msg: WireMsg::user(text),
        ts_ms: ts,
        stop: None,
        usage: None,
        model: None,
        tool_name: None,
        is_error: false,
        error_message: None,
    }
}

fn assistant_msg(text: &str, usage: Usage, stop: StopReason, ts: u64) -> EventMessage {
    EventMessage {
        msg: WireMsg::assistant(text),
        ts_ms: ts,
        stop: Some(stop),
        usage: Some(usage),
        model: Some("fake-model".to_string()),
        tool_name: None,
        is_error: false,
        error_message: None,
    }
}

fn tool_msg(call_id: &str, name: &str, text: &str, ts: u64) -> EventMessage {
    EventMessage {
        msg: WireMsg::tool_result(call_id, text),
        ts_ms: ts,
        stop: None,
        usage: None,
        model: None,
        tool_name: Some(name.to_string()),
        is_error: false,
        error_message: None,
    }
}

/// pi's `usage` object for the given oxa counts (cost is zero).
fn usage_obj(input: u64, output: u64, cache_read: u64) -> Value {
    json!({
        "input": input,
        "output": output,
        "cacheRead": cache_read,
        "cacheWrite": 0,
        "totalTokens": input + output + cache_read,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}
    })
}

/// each example of json.md field for field
#[test]
fn every_json_md_record_roundtrips() {
    let ts: u64 = 1733234401000;
    let user = user_msg("Review this repository", ts);
    let hello = assistant_msg(
        "Hello",
        Usage {
            prompt: 100,
            completion: 1,
            cached: 0,
            cached_missing: false,
        },
        StopReason::Stop,
        ts,
    );
    let result_msg = tool_msg("call_abc123", "bash", "complete output", ts);

    // (event, the json.md record it must produce)
    let cases: Vec<(AgentEvent, Value)> = vec![
        (AgentEvent::AgentStart, json!({"type": "agent_start"})),
        (AgentEvent::TurnStart, json!({"type": "turn_start"})),
        (
            AgentEvent::MessageStart {
                message: user.clone(),
            },
            json!({"type": "message_start", "message": {"role": "user", "content": "Review this repository", "timestamp": ts}}),
        ),
        (
            AgentEvent::MessageEnd {
                message: user.clone(),
            },
            json!({"type": "message_end", "message": {"role": "user", "content": "Review this repository", "timestamp": ts}}),
        ),
        (
            AgentEvent::AgentEnd {
                messages: vec![user.clone()],
                will_retry: false,
            },
            json!({"type": "agent_end", "messages": [{"role": "user", "content": "Review this repository", "timestamp": ts}], "willRetry": false}),
        ),
        (
            AgentEvent::AgentSettled { aborted: false },
            json!({"type": "agent_settled", "aborted": false}),
        ),
        (
            AgentEvent::MessageUpdate {
                usage: Usage {
                    prompt: 100,
                    completion: 1,
                    cached: 0,
                    cached_missing: false,
                },
                delta: MessageDelta::TextDelta {
                    index: 0,
                    delta: "Hello ".to_string(),
                },
            },
            json!({
                "type": "message_update",
                "usage": usage_obj(100, 1, 0),
                "assistantMessageEvent": {"type": "text_delta", "contentIndex": 0, "delta": "Hello "}
            }),
        ),
        (
            AgentEvent::TurnEnd {
                message: hello.clone(),
                tool_results: Vec::new(),
            },
            json!({
                "type": "turn_end",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "Hello"}],
                    "stopReason": "stop",
                    "usage": usage_obj(100, 1, 0),
                    "model": "fake-model",
                    "provider": "oxe",
                    "api": "openai-completions",
                    "timestamp": ts
                },
                "toolResults": []
            }),
        ),
        (
            AgentEvent::ToolExecutionStart {
                tool_call_id: "call_abc123".to_string(),
                tool_name: "bash".to_string(),
                args: json!({"command": "ls -la"}),
            },
            json!({"type": "tool_execution_start", "toolCallId": "call_abc123", "toolName": "bash", "args": {"command": "ls -la"}}),
        ),
        (
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: "call_abc123".to_string(),
                tool_name: "bash".to_string(),
                args: json!({"command": "ls -la"}),
                partial: "partial output".to_string(),
            },
            json!({
                "type": "tool_execution_update",
                "toolCallId": "call_abc123",
                "toolName": "bash",
                "args": {"command": "ls -la"},
                "partialResult": {"content": [{"type": "text", "text": "partial output"}], "details": {}}
            }),
        ),
        (
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "call_abc123".to_string(),
                tool_name: "bash".to_string(),
                result: ToolOutput::ok("complete output"),
                duration_ms: None,
            },
            json!({
                "type": "tool_execution_end",
                "toolCallId": "call_abc123",
                "toolName": "bash",
                "result": {"content": [{"type": "text", "text": "complete output"}], "details": {}},
                "isError": false
            }),
        ),
        (
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "call_abc123".to_string(),
                tool_name: "bash".to_string(),
                result: ToolOutput::error("boom"),
                duration_ms: Some(12),
            },
            json!({
                "type": "tool_execution_end",
                "toolCallId": "call_abc123",
                "toolName": "bash",
                "result": {"content": [{"type": "text", "text": "boom"}], "details": {}},
                "isError": true,
                "durationMs": 12
            }),
        ),
        (
            AgentEvent::CompactionStart {
                reason: CompactReason::Threshold,
            },
            json!({"type": "compaction_start", "reason": "threshold"}),
        ),
        (
            AgentEvent::CompactionEnd {
                reason: CompactReason::Threshold,
                result: Some(CompactionResult {
                    summary: "Summary of conversation...".to_string(),
                    first_kept_entry_id: "abc123".to_string(),
                    tokens_before: 150000,
                    estimated_tokens_after: 32000,
                    usage: Some(Usage {
                        prompt: 100,
                        completion: 1,
                        cached: 0,
                        cached_missing: false,
                    }),
                    details: json!({}),
                }),
                aborted: false,
                will_retry: false,
                error_message: None,
            },
            json!({
                "type": "compaction_end",
                "reason": "threshold",
                "result": {
                    "summary": "Summary of conversation...",
                    "firstKeptEntryId": "abc123",
                    "tokensBefore": 150000,
                    "estimatedTokensAfter": 32000,
                    "usage": usage_obj(100, 1, 0),
                    "details": {}
                },
                "aborted": false,
                "willRetry": false
            }),
        ),
        (
            AgentEvent::CompactionEnd {
                reason: CompactReason::Overflow,
                result: None,
                aborted: false,
                will_retry: false,
                error_message: Some("summary failed".to_string()),
            },
            json!({"type": "compaction_end", "reason": "overflow", "aborted": false, "willRetry": false, "errorMessage": "summary failed"}),
        ),
        (
            AgentEvent::AutoRetryStart {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 2000,
                error_message: "529 overloaded".to_string(),
            },
            json!({"type": "auto_retry_start", "attempt": 1, "maxAttempts": 3, "delayMs": 2000, "errorMessage": "529 overloaded"}),
        ),
        (
            AgentEvent::AutoRetryEnd {
                success: true,
                attempt: 2,
                final_error: None,
            },
            json!({"type": "auto_retry_end", "success": true, "attempt": 2}),
        ),
        (
            AgentEvent::AutoRetryEnd {
                success: false,
                attempt: 3,
                final_error: Some("gave up".to_string()),
            },
            json!({"type": "auto_retry_end", "success": false, "attempt": 3, "finalError": "gave up"}),
        ),
        (
            AgentEvent::SummarizationRetryScheduled {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 2000,
                error_message: "terminated".to_string(),
            },
            json!({"type": "summarization_retry_scheduled", "attempt": 1, "maxAttempts": 3, "delayMs": 2000, "errorMessage": "terminated"}),
        ),
        (
            AgentEvent::SummarizationRetryAttemptStart {
                source: "compaction",
                reason: Some(CompactReason::Threshold),
            },
            json!({"type": "summarization_retry_attempt_start", "source": "compaction", "reason": "threshold"}),
        ),
        (
            AgentEvent::SummarizationRetryAttemptStart {
                source: "branchSummary",
                reason: None,
            },
            json!({"type": "summarization_retry_attempt_start", "source": "branchSummary"}),
        ),
        (
            AgentEvent::SummarizationRetryFinished,
            json!({"type": "summarization_retry_finished"}),
        ),
        (
            AgentEvent::QueueUpdate {
                steering: vec!["a".to_string()],
                follow_up: Vec::new(),
            },
            json!({"type": "queue_update", "steering": ["a"], "followUp": []}),
        ),
        (
            AgentEvent::ThinkingLevelChanged {
                level: ThinkingLevel::High,
            },
            json!({"type": "thinking_level_changed", "level": "high"}),
        ),
        (
            AgentEvent::SessionInfoChanged {
                name: Some("my-feature-work".to_string()),
            },
            json!({"type": "session_info_changed", "name": "my-feature-work"}),
        ),
        (
            AgentEvent::SessionInfoChanged { name: None },
            json!({"type": "session_info_changed"}),
        ),
        (
            AgentEvent::BashExecutionUpdate {
                id: Some("req-1".to_string()),
                delta: "total 48\n".to_string(),
            },
            json!({"type": "bash_execution_update", "id": "req-1", "delta": "total 48\n"}),
        ),
        (
            AgentEvent::ExtensionError {
                extension_path: "/path/to/extension.ts".to_string(),
                event: "tool_call".to_string(),
                error: "Error message".to_string(),
            },
            json!({"type": "extension_error", "extensionPath": "/path/to/extension.ts", "event": "tool_call", "error": "Error message"}),
        ),
    ];
    for (event, expected) in cases {
        assert_eq!(event_to_json(&event), expected, "record of {}", event.name());
    }

    // The tool result message of json.md's tool lifecycle (toolResult role).
    assert_eq!(
        message_to_json(&result_msg),
        json!({
            "role": "toolResult",
            "toolCallId": "call_abc123",
            "toolName": "bash",
            "content": [{"type": "text", "text": "complete output"}],
            "isError": false,
            "timestamp": ts
        })
    );

    // The session header of json.md, field for field.
    let header = SessionHeader {
        version: 3,
        id: "uuid".to_string(),
        ts_ms: 1733234400000,
        cwd: "/path".into(),
        parent_session: None,
    };
    assert_eq!(
        session_header_record(&header),
        json!({"type": "session", "version": 3, "id": "uuid", "timestamp": "2024-12-03T14:00:00.000Z", "cwd": "/path"})
    );
}

/// no partial
#[test]
fn message_update_is_delta_only() {
    let usage = Usage {
        prompt: 100,
        completion: 1,
        cached: 0,
        cached_missing: false,
    };
    let deltas = vec![
        MessageDelta::TextStart { index: 0 },
        MessageDelta::TextDelta {
            index: 0,
            delta: "a".to_string(),
        },
        MessageDelta::TextEnd {
            index: 0,
            content: "a".to_string(),
        },
        MessageDelta::ThinkingStart { index: 0 },
        MessageDelta::ThinkingDelta {
            index: 0,
            delta: "t".to_string(),
        },
        MessageDelta::ThinkingEnd {
            index: 0,
            content: "t".to_string(),
        },
        MessageDelta::ToolcallStart {
            index: 1,
            id: "call_1".to_string(),
            name: "read".to_string(),
        },
        MessageDelta::ToolcallDelta {
            index: 1,
            delta: "{\"path\"".to_string(),
        },
        MessageDelta::ToolcallEnd {
            index: 1,
            call: ToolCall {
                id: "call_1".to_string(),
                name: "read".to_string(),
                args_raw: "{\"path\":\"a\"}".to_string(),
            },
        },
    ];
    for delta in deltas {
        let event = AgentEvent::MessageUpdate { usage, delta };
        let v = event_to_json(&event);
        let obj = v.as_object().expect("a record is an object");
        assert!(
            !obj.contains_key("message"),
            "message_update carries the whole message: {v}"
        );
        assert!(
            !obj.contains_key("partial"),
            "message_update carries a partial snapshot: {v}"
        );
        let inner = obj["assistantMessageEvent"]
            .as_object()
            .expect("the delta is an object");
        assert!(
            !inner.contains_key("partial"),
            "the delta carries a partial snapshot: {v}"
        );
    }

    // The toolcall_start record names the call without its snapshot.
    let start = event_to_json(&AgentEvent::MessageUpdate {
        usage,
        delta: MessageDelta::ToolcallStart {
            index: 1,
            id: "call_1".to_string(),
            name: "read".to_string(),
        },
    });
    assert_eq!(
        start["assistantMessageEvent"],
        json!({"type": "toolcall_start", "contentIndex": 1, "id": "call_1", "toolName": "read"})
    );

    // In a real run the stream is delta-only too.
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("Hello there")]));
    let mut session = open(&env, &engine);
    let mut out = Vec::new();
    let mut err = Vec::new();
    run_json(&mut session, prompt("hi"), &mut out, &mut err).expect("the run completes");
    let updates: Vec<Value> = records(&out)
        .into_iter()
        .filter(|r| r["type"] == "message_update")
        .collect();
    assert!(
        !updates.is_empty(),
        "the streamed reply produces message_update records"
    );
    for r in updates {
        let obj = r.as_object().unwrap();
        assert!(!obj.contains_key("message"));
        assert!(!r["assistantMessageEvent"]
            .as_object()
            .unwrap()
            .contains_key("partial"));
    }
}

/// no CR, one record per line
#[test]
fn lf_only_framing() {
    // The record writer itself: one line, LF only, even when the strings carry CR, LF and U+2028.
    let v = json!({"type": "notice", "text": "a\r\nb\u{2028}c"});
    let mut buf = Vec::new();
    write_record(&mut buf, &v).expect("a record writes");
    assert_eq!(buf.iter().filter(|b| **b == b'\n').count(), 1);
    assert_eq!(*buf.last().unwrap(), b'\n');
    assert!(!buf.contains(&b'\r'), "a record contains a CR");
    let back: Value = serde_json::from_slice(&buf[..buf.len() - 1]).expect("the line is one JSON object");
    assert_eq!(back, v);

    // A whole run: every record is its own LF-terminated line and no CR appears on stdout.
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text(
        "line one\r\nline two\u{2028}end",
    )]));
    let mut session = open(&env, &engine);
    let mut out = Vec::new();
    let mut err = Vec::new();
    run_json(&mut session, prompt("first\r\nsecond"), &mut out, &mut err).expect("the run completes");
    assert!(!out.contains(&b'\r'), "stdout contains a CR");
    assert_eq!(out.last(), Some(&b'\n'));
    let parsed = records(&out);
    let lines = out.iter().filter(|b| **b == b'\n').count();
    assert_eq!(parsed.len(), lines, "one record per line");
    assert!(parsed.iter().any(|r| r["type"] == "agent_end"));
}

/// stdout has only the last assistant text
#[test]
fn print_final_text_only() {
    let env = TestEnv::new();
    let turns = vec![
        ScriptTurn {
            reasoning: Some("pondering".to_string()),
            ..ScriptTurn::text("first answer")
        },
        ScriptTurn {
            reasoning: Some("more thought".to_string()),
            ..ScriptTurn::text("final answer")
        },
    ];
    let engine = Arc::new(ScriptedEngine::new(turns));
    let mut session = open(&env, &engine);
    let prompts = vec![("one".to_string(), Vec::new()), ("two".to_string(), Vec::new())];
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_print(&mut session, prompts, &mut out, &mut err).expect("the run completes");
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(out).unwrap(), "final answer\n");
}

/// No tools declared and the final text is tool-call markup: one stderr warning, exit code 0.
#[test]
fn print_warns_on_tool_markup_without_tools() {
    const WARNING: &str = "warning: the model answered with tool-call markup but no tools are available (--no-tools); the answer may be unusable\n";
    let env = TestEnv::new();
    let markup = "<tool_calls><invoke name=\"exec_command\"></invoke></tool_calls>";
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text(markup)]));
    let mut session = open(&env, &engine);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_print(&mut session, prompt("review"), &mut out, &mut err).unwrap();
    assert_eq!(code, 0);
    assert_eq!(String::from_utf8(out).unwrap(), format!("{markup}\n"));
    assert_eq!(String::from_utf8(err).unwrap(), WARNING);

    // Plain text: no warning.
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("fine")]));
    let mut session = open(&env, &engine);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    run_print(&mut session, prompt("review"), &mut out, &mut err).unwrap();
    assert!(err.is_empty());
}

/// piped text + prompt
#[test]
fn stdin_prepend() {
    // The piped text is prepended by the caller (the CLI) to the first prompt; the session must
    // send that text unchanged as the user message of the request.
    let piped = "error log line 1\nerror log line 2";
    let prompt_text = "explain these errors";
    let combined = format!("{piped}\n\n{prompt_text}");

    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text(
        "they are about a missing file",
    )]));
    let mut session = open(&env, &engine);
    let mut out = Vec::new();
    let mut err = Vec::new();
    run_print(
        &mut session,
        vec![(combined.clone(), Vec::new())],
        &mut out,
        &mut err,
    )
    .expect("the run completes");

    let requests = engine.requests();
    assert_eq!(requests.len(), 1);
    let last_user = requests[0]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == oxa_proto::Role::User)
        .expect("the request carries the user message");
    assert_eq!(last_user.content.as_deref(), Some(combined.as_str()));
    assert_eq!(String::from_utf8(out).unwrap(), "they are about a missing file\n");
}

/// record shape
#[test]
fn session_header_first_in_json_mode() {
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("ok")]));
    let mut session = open(&env, &engine);
    let state = session.state();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_json(&mut session, prompt("hello"), &mut out, &mut err).expect("the run completes");
    assert_eq!(code, 0);

    let parsed = records(&out);
    let header = &parsed[0];
    assert_eq!(header["type"], "session");
    assert_eq!(header["version"], 3);
    assert_eq!(header["id"], state.session_id.as_str());
    assert_eq!(header["cwd"], env.work().to_string_lossy().as_ref());
    assert!(header["timestamp"].is_string());
    assert!(
        parsed.iter().skip(1).any(|r| r["type"] == "agent_start"),
        "events follow the header"
    );
    assert!(
        parsed.iter().skip(1).all(|r| r["type"] != "session"),
        "the header is written once"
    );
}

/// input = prompt - cached, cacheRead = cached
#[test]
fn usage_mapping() {
    // The mapping itself.
    let usage = Usage {
        prompt: 150,
        completion: 20,
        cached: 100,
        cached_missing: false,
    };
    let m = assistant_msg("x", usage, StopReason::Stop, 1);
    let v = message_to_json(&m);
    assert_eq!(v["usage"], usage_obj(50, 20, 100));
    assert_eq!(v["usage"]["input"], 50);
    assert_eq!(v["usage"]["cacheRead"], 100);
    assert_eq!(v["usage"]["totalTokens"], 170);

    // In a real run the engine's counts reach the record through the same mapping.
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("counted")]));
    let mut session = open(&env, &engine);
    let mut out = Vec::new();
    let mut err = Vec::new();
    run_json(&mut session, prompt("count me"), &mut out, &mut err).expect("the run completes");
    let reported = engine.requests()[0].usage.expect("the engine reports usage");
    let end = records(&out)
        .into_iter()
        .find(|r| r["type"] == "message_end" && r["message"]["role"] == "assistant")
        .expect("the assistant message ends");
    assert_eq!(
        end["message"]["usage"]["input"],
        reported.prompt - reported.cached
    );
    assert_eq!(end["message"]["usage"]["cacheRead"], reported.cached);
    assert_eq!(end["message"]["usage"]["output"], reported.completion);
}

/// 0 / 1
#[test]
fn exit_codes() {
    // A completed run: 0, in print and in json mode.
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("fine")]));
    let mut session = open(&env, &engine);
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        run_print(&mut session, prompt("ok"), &mut out, &mut err).unwrap(),
        0
    );

    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(vec![ScriptTurn::text("fine")]));
    let mut session = open(&env, &engine);
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        run_json(&mut session, prompt("ok"), &mut out, &mut err).unwrap(),
        0
    );

    // A failed run (the script has no reply for it): 1, nothing on stdout in print mode, the
    // reason on stderr.
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut session = open(&env, &engine);
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        run_print(&mut session, prompt("ok"), &mut out, &mut err).unwrap(),
        1
    );
    assert!(out.is_empty(), "a failed print run writes nothing to stdout");
    assert!(!err.is_empty(), "the failure is reported on stderr");

    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let mut session = open(&env, &engine);
    let mut out = Vec::new();
    let mut err = Vec::new();
    assert_eq!(
        run_json(&mut session, prompt("ok"), &mut out, &mut err).unwrap(),
        1
    );
    let parsed = records(&out);
    assert!(
        parsed.iter().any(|r| r["type"] == "agent_settled"),
        "the settled record is still written"
    );
}

/// A spent daily cloud budget (gateway 429) is a failed run with its own exit code, in print
/// and json mode, and the reason is on stderr.
#[test]
fn budget_exhausted_has_its_own_exit_code() {
    let fault = || {
        Arc::new(ScriptedEngine::new(vec![ScriptTurn::fault(Fault::Http {
            status: 429,
            body: "The daily cloud budget is exhausted.".to_string(),
        })]))
    };
    let env = TestEnv::new();
    let engine = fault();
    let mut session = open(&env, &engine);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_print(&mut session, prompt("ok"), &mut out, &mut err).unwrap();
    assert_eq!(code, 3);
    assert!(String::from_utf8_lossy(&err).contains("budget"), "{err:?}");

    let env = TestEnv::new();
    let engine = fault();
    let mut session = open(&env, &engine);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(
        run_json(&mut session, prompt("ok"), &mut out, &mut err).unwrap(),
        3
    );
}

/// prefill, cache_miss shapes
#[test]
fn own_events_additive() {
    assert_eq!(
        event_to_json(&AgentEvent::Prefill {
            done: 10,
            total: 100,
            secs: 2
        }),
        json!({"type": "prefill", "done": 10, "total": 100, "secs": 2})
    );
    let epoch = event_to_json(&AgentEvent::PrefixEpoch {
        sha256: [0xab; 32],
        reason: "model".to_string(),
        reprefill_tokens: 500,
    });
    assert_eq!(
        epoch,
        json!({"type": "prefix_epoch", "sha256": "ab".repeat(32), "reason": "model", "reprefillTokens": 500})
    );
    assert_eq!(
        event_to_json(&AgentEvent::CacheMiss {
            expected: 120,
            cached: 0,
            cause: oxa_proto::miss_cause::COLD_START
        }),
        json!({"type": "cache_miss", "expected": 120, "cached": 0, "cause": "cold_start"})
    );
    assert_eq!(
        event_to_json(&AgentEvent::ModeChanged { mode: Mode::Plan }),
        json!({"type": "mode_changed", "mode": "plan"})
    );
    assert_eq!(
        event_to_json(&AgentEvent::Notice {
            level: Level::Warning,
            text: "epoch cost".to_string()
        }),
        json!({"type": "notice", "level": "warning", "text": "epoch cost"})
    );

    // get_state: pi's names plus oxa's mode and prefixSha256.
    let env = TestEnv::new();
    let engine = Arc::new(ScriptedEngine::new(Vec::new()));
    let session = open(&env, &engine);
    let state = state_to_json(&session.state());
    assert_eq!(state["mode"], "workspace");
    // pi's default thinking level (settings.md `defaultThinkingLevel`).
    assert_eq!(state["thinkingLevel"], "medium");
    assert_eq!(state["steeringMode"], "one-at-a-time");
    assert_eq!(state["followUpMode"], "one-at-a-time");
    assert_eq!(state["prefixSha256"].as_str().map(str::len), Some(64));
    assert_eq!(state["sessionId"], session.state().session_id.as_str());
}

/// Every entry shape is the object the session file stores (the codec is the one source).
#[test]
fn entry_json_is_the_stored_object() {
    let entry = Entry {
        id: "1a2b3c4d".to_string(),
        parent: None,
        ts_ms: 1733234400000,
        body: EntryBody::Msg {
            wire: WireMsg::user("hello"),
            parts: Vec::new(),
        },
    };
    let mut line = Vec::new();
    encode_entry(&entry, &mut line);
    let stored: Value = serde_json::from_slice(&line).unwrap();
    assert_eq!(entry_to_json(&entry), stored);
    assert_eq!(stored["type"], "message");
    assert_eq!(stored["timestamp"], "2024-12-03T14:00:00.000Z");
}

/// The sink writes one record per event and keeps accepting events after a failed write.
#[test]
fn json_sink_writes_records_and_keeps_going() {
    struct Broken;
    impl std::io::Write for Broken {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("closed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = JsonSink {
        out: Broken,
        error: None,
    };
    sink.emit(&AgentEvent::AgentStart);
    sink.emit(&AgentEvent::TurnStart);
    assert!(sink.error.is_some(), "the first write error is kept");

    let mut ok = JsonSink {
        out: Vec::new(),
        error: None,
    };
    ok.emit(&AgentEvent::AgentStart);
    ok.emit(&AgentEvent::AgentSettled { aborted: true });
    let text = String::from_utf8(ok.out).unwrap();
    assert_eq!(
        text,
        "{\"type\":\"agent_start\"}\n{\"type\":\"agent_settled\",\"aborted\":true}\n"
    );
}

/// A writer that records every write and flush in one shared, ordered log.
struct Logged {
    tag: &'static str,
    log: Arc<std::sync::Mutex<Vec<String>>>,
}

impl std::io::Write for Logged {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:{}", self.tag, String::from_utf8_lossy(b)));
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.log.lock().unwrap().push(format!("{}:<flush>", self.tag));
        Ok(())
    }
}

/// Progress goes to stderr in order, flushed per event, before the final text on stdout.
#[test]
fn print_streams_progress_to_stderr_and_final_text_to_stdout() {
    let env = TestEnv::new();
    let turns = vec![
        ScriptTurn {
            text: Some("let me look".to_string()),
            ..ScriptTurn::tool("c1", "bash", r#"{"command":"ls\n -la"}"#)
        },
        ScriptTurn::text("done"),
    ];
    let engine = Arc::new(ScriptedEngine::new(turns));
    let mut session = open(&env, &engine);
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut out = Logged {
        tag: "out",
        log: log.clone(),
    };
    let mut err = Logged {
        tag: "err",
        log: log.clone(),
    };
    let code = run_print(&mut session, prompt("go"), &mut out, &mut err).unwrap();
    assert_eq!(code, 0);
    let log = log.lock().unwrap().clone();
    let pos = |needle: &str| {
        log.iter()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} missing in {log:?}"))
    };
    let (text, start, end, fin) = (
        pos("err:let me look"),
        pos("err:[tool] bash\n"),
        pos("err:[tool] bash failed"),
        pos("out:done"),
    );
    assert!(text < start && start < end && end < fin, "{log:?}");
    // Each progress write is flushed right after it, and so is the reply.
    for i in [text, start, end] {
        assert_eq!(log[i + 1], "err:<flush>", "{log:?}");
    }
    assert_eq!(log[fin + 2], "out:<flush>", "{log:?}");
    let stdout: String = log
        .iter()
        .filter_map(|l| l.strip_prefix("out:"))
        .filter(|l| *l != "<flush>")
        .collect();
    assert_eq!(stdout, "done\n");
}
