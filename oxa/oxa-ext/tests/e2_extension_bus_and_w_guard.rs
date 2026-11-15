//! E2 acceptance tests: Extension bus and W-guard.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_ext::guard::{self, GuardVerdict};
use oxa_ext::{reply_from_js, ExtensionHost, LoadOpts};
use oxa_fake::TestEnv;
use oxa_proto::{codes, HeadlessUi, HookPoint, HookReply, Hooks, Value, WireMsg};
use oxa_script::Limits;
use serde_json::json;
use std::time::{Duration, Instant};

/// A module whose default export installs `body` on the `oxa` object.
fn ext(body: &str) -> String {
    format!("export default function (oxa) {{ {body} }}")
}

/// A host with a working directory under a fresh test environment.
fn host() -> (TestEnv, ExtensionHost) {
    let env = TestEnv::new();
    let host = ExtensionHost::new(env.work()).expect("start the extension runtime");
    (env, host)
}

fn user(text: &str) -> WireMsg {
    WireMsg::user(text)
}

/// load order, per event composition
#[test]
fn handler_order_as_extensions_md() {
    let (_env, host) = host();
    let opts = LoadOpts::default();
    // Extension a registers two handlers (registration order), b one more; each appends.
    let a = host.load_source(
        "a",
        &ext(
            r#"oxa.on("input", (e) => ({action: "transform", text: e.text + "+a1"}));
                oxa.on("input", (e) => ({action: "transform", text: e.text + "+a2"}));"#,
        ),
        &opts,
    );
    let b = host.load_source(
        "b",
        &ext(r#"oxa.on("input", (e) => ({action: "transform", text: e.text + "+b"}));"#),
        &opts,
    );
    assert!(a.error.is_none(), "a must load: {:?}", a.error);
    assert!(b.error.is_none(), "b must load: {:?}", b.error);
    assert_eq!(a.id.0 + 1, b.id.0, "load order is the id order");

    let mut ui = HeadlessUi::default();
    let reply = host.fire(HookPoint::Input, &json!({ "text": "x" }), &mut ui);
    assert_eq!(reply, HookReply::ReplaceInput("x+a1+a2+b".to_string()));
    assert!(host.state().take_errors().is_empty());
}

/// transforming replies chain, cancel stops
#[test]
fn composition_rules() {
    let (_env, host) = host();
    let opts = LoadOpts::default();
    // Transforming replies chain: the second handler sees the transformed arguments.
    let first = host.load_source(
        "first",
        &ext(r#"oxa.on("tool_call", (e) => ({input: {n: 1}}));"#),
        &opts,
    );
    let second = host.load_source(
        "second",
        &ext(r#"oxa.on("tool_call", (e) => ({input: {n: e.input.n + 1}}));"#),
        &opts,
    );
    assert!(first.error.is_none() && second.error.is_none());
    let mut ui = HeadlessUi::default();
    let payload = json!({"toolName": "t", "toolCallId": "c1", "input": {}});
    assert_eq!(
        host.fire(HookPoint::ToolCall, &payload, &mut ui),
        HookReply::ReplaceArgs("{\"n\":2}".to_string())
    );

    // A blocking reply stops the chain: the handler after it never runs.
    let (_env2, stopper) = self::host();
    let blocker = stopper.load_source(
        "blocker",
        &ext(r#"oxa.on("tool_call", (e) => ({block: true, reason: "nope"}));"#),
        &opts,
    );
    let never = stopper.load_source(
        "never",
        &ext(r#"oxa.on("tool_call", (e) => { throw new Error("reached"); });"#),
        &opts,
    );
    assert!(blocker.error.is_none() && never.error.is_none());
    assert_eq!(
        stopper.fire(HookPoint::ToolCall, &payload, &mut ui),
        HookReply::Block("nope".to_string())
    );
    assert!(
        stopper.state().take_errors().is_empty(),
        "the chain must stop before `never`"
    );

    // A cancelling reply of a session point stops the chain too.
    let (_env3, canceller) = self::host();
    let c = canceller.load_source(
        "c",
        &ext(r#"oxa.on("session_before_compact", () => ({cancel: true, reason: "stop"}));"#),
        &opts,
    );
    let d = canceller.load_source(
        "d",
        &ext(r#"oxa.on("session_before_compact", () => { throw new Error("reached"); });"#),
        &opts,
    );
    assert!(c.error.is_none() && d.error.is_none());
    assert_eq!(
        canceller.fire(HookPoint::SessionBeforeCompact, &json!({}), &mut ui),
        HookReply::Cancel("stop".to_string())
    );
    assert!(canceller.state().take_errors().is_empty());
}

/// guard_refused
#[test]
fn guard_non_prefix_context_refused() {
    // Pure: a list that does not extend the wire is refused unless declared.
    let wire = vec![user("hi")];
    match guard::check(
        HookPoint::Context,
        &wire,
        HookReply::Rebase(vec![user("other")]),
        false,
    ) {
        GuardVerdict::Refused(e) => assert_eq!(e.code, codes::GUARD_REFUSED),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // Through the bus: the reply is Continue and the refusal is an extension error.
    let (_env, host) = host();
    let loaded = host.load_source(
        "rewriter",
        &ext(r#"oxa.on("context", () => ({messages: [{role: "user", content: "other"}]}));"#),
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none());
    let mut ui = HeadlessUi::default();
    let payload = json!({"messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(
        host.fire(HookPoint::Context, &payload, &mut ui),
        HookReply::Continue
    );
    let errors = host.state().take_errors();
    assert_eq!(errors.len(), 1, "one refusal expected: {errors:?}");
    assert_eq!(errors[0].event, "context");
    assert!(errors[0].error.contains(codes::GUARD_REFUSED), "{:?}", errors[0]);
}

/// silent
#[test]
fn guard_prefix_extension_accepted_as_append() {
    // Pure: a list that extends the wire is a pure append.
    let wire = vec![user("hi")];
    let extended = vec![user("hi"), user("extra")];
    assert_eq!(
        guard::check(
            HookPoint::Context,
            &wire,
            HookReply::Rebase(extended.clone()),
            false
        ),
        GuardVerdict::Pass(HookReply::Rebase(extended.clone()))
    );

    // Through the bus: returned as a rebase reply, with no error and no log.
    let (_env, host) = host();
    let loaded = host.load_source(
        "appender",
        &ext(r#"oxa.on("context", (e) => ({messages: [...e.messages, {role: "user", content: "extra"}]}));"#),
        &LoadOpts::default(),
    );
    assert!(loaded.error.is_none());
    let mut ui = HeadlessUi::default();
    let payload = json!({"messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(
        host.fire(HookPoint::Context, &payload, &mut ui),
        HookReply::Rebase(extended)
    );
    assert!(host.state().take_errors().is_empty());
}

/// miss cause extension
#[test]
fn declared_rebase_logged() {
    // Pure: a non-prefix list from a declared extension is a declared rebase.
    let wire = vec![user("hi")];
    let other = vec![user("other")];
    assert_eq!(
        guard::check(HookPoint::Context, &wire, HookReply::Rebase(other.clone()), true),
        GuardVerdict::DeclaredRebase(other.clone())
    );

    // Through the bus: the extension declares `rebase: true`; the rebase reply reaches the core
    // and no error is raised.
    let (_env, host) = host();
    let source = format!(
        "export const manifest = {{ rebase: true }};\n{}",
        ext(r#"oxa.on("context", () => ({messages: [{role: "user", content: "other"}]}));"#)
    );
    let loaded = host.load_source("summarizer", &source, &LoadOpts::default());
    assert!(loaded.error.is_none(), "{:?}", loaded.error);
    assert!(loaded.declared_rebase, "the manifest's rebase flag is kept");
    let mut ui = HeadlessUi::default();
    let payload = json!({"messages": [{"role": "user", "content": "hi"}]});
    // E-INT 3: the declared rebase reaches the core as `DeclaredRebase` with its origin (the
    // reply path's origin flag), and the log sink gets a line naming the miss cause.
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = lines.clone();
    host.state()
        .set_log(Some(std::sync::Arc::new(move |_level, line: &str| {
            sink.lock().unwrap().push(line.to_string())
        })));
    assert_eq!(
        host.fire(HookPoint::Context, &payload, &mut ui),
        HookReply::DeclaredRebase {
            messages: other,
            origin: loaded.path.clone(),
        }
    );
    assert!(host.state().take_errors().is_empty());
    let lines = lines.lock().unwrap();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("declared rebase") && lines[0].contains("cause: extension"));
}

/// Observer points
#[test]
fn observer_reply_ignored() {
    // Pure: anything but Continue from an observer is ignored.
    assert!(matches!(
        guard::check(HookPoint::AgentStart, &[], HookReply::Cancel("x".into()), false),
        GuardVerdict::Ignored { .. }
    ));

    // Through the bus: the reply is dropped, and the next observer still runs.
    let (_env, host) = host();
    let opts = LoadOpts::default();
    let first = host.load_source(
        "loud",
        &ext(r#"oxa.on("agent_start", () => ({cancel: true, reason: "x"}));"#),
        &opts,
    );
    let second = host.load_source(
        "after",
        &ext(r#"oxa.on("agent_start", () => { throw new Error("reached"); });"#),
        &opts,
    );
    assert!(first.error.is_none() && second.error.is_none());
    let mut ui = HeadlessUi::default();
    assert_eq!(
        host.fire(HookPoint::AgentStart, &Value::Null, &mut ui),
        HookReply::Continue
    );
    let errors = host.state().take_errors();
    assert_eq!(errors.len(), 1, "the second observer must run: {errors:?}");
    assert!(errors[0].error.contains("reached"));
}

/// Continue
#[test]
fn handler_throw_is_extension_error() {
    let (_env, host) = host();
    let opts = LoadOpts::default();
    let thrower = host.load_source(
        "thrower",
        &ext(r#"oxa.on("input", () => { throw new Error("boom"); });"#),
        &opts,
    );
    let handler = host.load_source(
        "handler",
        &ext(r#"oxa.on("input", () => ({action: "handled"}));"#),
        &opts,
    );
    assert!(thrower.error.is_none() && handler.error.is_none());
    let mut ui = HeadlessUi::default();
    // The throw counts as Continue, so the chain goes on to the next handler.
    assert_eq!(
        host.fire(HookPoint::Input, &json!({"text": "x"}), &mut ui),
        HookReply::Handled
    );
    let errors = host.state().take_errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].event, "input");
    assert_eq!(errors[0].extension_path, thrower.path);
    assert!(errors[0].error.contains("boom"), "{:?}", errors[0]);
}

/// runaway handler interrupted
#[test]
fn deadline_per_handler() {
    let (_env, host) = host();
    let opts = LoadOpts {
        limits: Limits {
            deadline_ms: Some(300),
            ..Limits::EXTENSION
        },
        flag_values: Vec::new(),
    };
    let runaway = host.load_source(
        "runaway",
        &ext(r#"oxa.on("input", () => { while (true) {} });"#),
        &opts,
    );
    let after = host.load_source(
        "after",
        &ext(r#"oxa.on("input", () => ({action: "handled"}));"#),
        &opts,
    );
    assert!(runaway.error.is_none() && after.error.is_none());
    let mut ui = HeadlessUi::default();
    let started = Instant::now();
    let reply = host.fire(HookPoint::Input, &json!({"text": "x"}), &mut ui);
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the handler was not interrupted in time"
    );
    assert_eq!(reply, HookReply::Handled);
    let errors = host.state().take_errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].extension_path, runaway.path);
}

/// Pure mapping of what a handler returns to a reply (pi's result shapes).
#[test]
fn reply_shapes() {
    assert_eq!(
        reply_from_js(HookPoint::Input, &json!({"action": "transform", "text": "t"})),
        HookReply::ReplaceInput("t".into())
    );
    assert_eq!(
        reply_from_js(HookPoint::Input, &json!({"action": "handled"})),
        HookReply::Handled
    );
    assert_eq!(
        reply_from_js(HookPoint::ToolCall, &json!({"block": true, "reason": "no"})),
        HookReply::Block("no".into())
    );
    assert_eq!(
        reply_from_js(
            HookPoint::BeforeAgentStart,
            &json!({"message": {"content": "hey"}})
        ),
        HookReply::Append(vec![WireMsg::user("hey")])
    );
    assert_eq!(
        reply_from_js(
            HookPoint::Context,
            &json!({"messages": [{"role": "user", "content": "a"}]})
        ),
        HookReply::Rebase(vec![WireMsg::user("a")])
    );
    assert_eq!(
        reply_from_js(HookPoint::ProjectTrust, &json!({"trust": true})),
        HookReply::Trust(true)
    );
    assert_eq!(
        reply_from_js(HookPoint::AgentStart, &json!({"x": 1})),
        HookReply::Continue
    );
    assert_eq!(reply_from_js(HookPoint::Input, &Value::Null), HookReply::Continue);
}
