//! Wave H (lane hooks): pi's result shapes of the newly fired points and `wants` from the
//! registrations, through a real extension host.
use oxa_ext::{reply_from_js, ExtensionHost, LoadOpts};
use oxa_fake::TestEnv;
use oxa_proto::{HeadlessUi, HookPoint, HookReply, Hooks, WireMsg};
use serde_json::json;

#[test]
fn project_trust_takes_pis_answer() {
    let p = HookPoint::ProjectTrust;
    assert_eq!(
        reply_from_js(p, &json!({"trusted": "yes"})),
        HookReply::Trust(true)
    );
    assert_eq!(
        reply_from_js(p, &json!({"trusted": "no"})),
        HookReply::Trust(false)
    );
    assert_eq!(
        reply_from_js(p, &json!({"trusted": "yes", "remember": true})),
        HookReply::TrustRemembered(true)
    );
    assert_eq!(
        reply_from_js(p, &json!({"trusted": "undecided"})),
        HookReply::Continue
    );
    assert_eq!(reply_from_js(p, &json!({"trust": true})), HookReply::Trust(true));
}

#[test]
fn before_settle_appends_custom_message_drafts_only() {
    let p = HookPoint::AgentBeforeSettle;
    let r = reply_from_js(
        p,
        &json!({"entries": [
            {"type": "custom", "customType": "x", "data": 1},
            {"type": "custom_message", "customType": "y", "content": "again"},
        ]}),
    );
    assert_eq!(r, HookReply::Append(vec![WireMsg::user("again")]));
    assert_eq!(reply_from_js(p, &json!({"continue": true})), HookReply::Continue);
}

#[test]
fn wants_follows_registrations() {
    let env = TestEnv::new();
    let path = env.write(
        "work/e.js",
        "export default function (oxa) { oxa.on('message_update', (e, ctx) => {}); oxa.on('agent_end', (e, ctx) => {}); }",
    );
    let host = ExtensionHost::new(env.work()).expect("host");
    let loaded = host.load(&[path], &LoadOpts::default());
    assert!(
        loaded.iter().all(|e| e.error.is_none()),
        "{:?}",
        loaded.iter().map(|e| &e.error).collect::<Vec<_>>()
    );
    assert!(host.wants(HookPoint::MessageUpdate));
    assert!(host.wants(HookPoint::AgentEnd));
    assert!(!host.wants(HookPoint::ProviderStreamEvent));
    assert!(!host.wants(HookPoint::ToolExecutionUpdate));
    let mut ui = HeadlessUi::default();
    // An observer point: the reply is ignored, and nothing is recorded as an error.
    assert_eq!(
        host.fire(HookPoint::MessageUpdate, &json!({"message": {}}), &mut ui),
        HookReply::Continue
    );
    assert!(host.state().take_errors().is_empty());
}
