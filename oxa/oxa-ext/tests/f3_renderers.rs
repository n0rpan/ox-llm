//! F3 acceptance tests: Renderers.
//! One test per acceptance bullet of WAVES.toml. Remove `#[ignore]` as you implement; the task is
//! done when every test here is un-ignored and green. Add tests freely; do not delete or rename these.
//! Isolation: use `oxa_fake::TestEnv` for every file and directory; never touch the real home.

use oxa_ext::{ExtensionHost, LoadOpts};
use oxa_fake::TestEnv;
use oxa_proto::{Hooks, RenderKind};
use serde_json::json;

/// Wrap a body as an extension module (`export default function (oxa) { ... }`).
fn ext(body: &str) -> String {
    format!("export default function (oxa) {{ {body} }}")
}

/// A host with one loaded extension (`name`) whose module is `body`.
fn host_with(body: &str) -> (TestEnv, ExtensionHost) {
    let env = TestEnv::new();
    let host = ExtensionHost::new(env.work()).expect("runtime thread");
    let loaded = host.load_source("f3", &ext(body), &LoadOpts::default());
    assert!(loaded.error.is_none(), "load failed: {:?}", loaded.error);
    (env, host)
}

/// custom_message drawn by JS
#[test]
fn message_renderer() {
    let (_env, host) = host_with(
        r#"oxa.registerMessageRenderer("fancy", (msg, opts) => [
            "* " + msg.content + " (w" + opts.width + (opts.expanded ? ", expanded" : "") + ")"
        ]);"#,
    );
    let payload = json!({"customType": "fancy", "content": "hi", "details": null, "expanded": true});
    let lines = host.render(RenderKind::Message, &payload, 40);
    assert_eq!(lines, Some(vec!["* hi (w40, expanded)".to_string()]));

    // A multi-line answer is returned line by line.
    let (_env2, host2) = host_with(r#"oxa.registerMessageRenderer("two", () => ["a", "b"]);"#);
    let lines = host2.render(RenderKind::Message, &json!({"customType": "two"}), 10);
    assert_eq!(lines, Some(vec!["a".to_string(), "b".to_string()]));

    // Last registration for a customType wins.
    let (_env3, host3) = host_with(
        r#"oxa.registerMessageRenderer("k", () => ["first"]);
           oxa.registerMessageRenderer("k", () => ["second"]);"#,
    );
    let lines = host3.render(RenderKind::Message, &json!({"customType": "k"}), 10);
    assert_eq!(lines, Some(vec!["second".to_string()]));
    assert!(host3.state().take_errors().is_empty());
}

/// custom entry
#[test]
fn entry_renderer() {
    let (_env, host) = host_with(
        r#"oxa.registerEntryRenderer("stat", (entry, opts) => ["entry " + entry.data.n + " @" + opts.width]);"#,
    );
    let payload = json!({"customType": "stat", "data": {"n": 7}});
    assert_eq!(
        host.render(RenderKind::Entry, &payload, 80),
        Some(vec!["entry 7 @80".to_string()])
    );
    // A message renderer does not answer for an entry of the same customType.
    assert_eq!(host.render(RenderKind::Message, &payload, 80), None);
}

/// expanded and partial
#[test]
fn tool_call_and_result_renderers() {
    let (_env, host) = host_with(
        r#"oxa.registerToolRenderer((name) => {
            if (name !== "lookup") return undefined;
            return {
                renderCall: (args, ctx) => ["call " + args.q + " w" + ctx.width],
                renderResult: (result, opts, ctx) => [
                    "result " + result + (opts.expanded ? " full" : " short") + (opts.partial ? " partial" : "")
                ],
            };
        });"#,
    );
    let call = json!({"toolName": "lookup", "args": {"q": "x"}});
    assert_eq!(
        host.render(RenderKind::ToolCall, &call, 30),
        Some(vec!["call x w30".to_string()])
    );
    let result = json!({
        "toolName": "lookup", "args": {"q": "x"}, "result": "42",
        "isError": false, "expanded": true, "partial": true,
    });
    assert_eq!(
        host.render(RenderKind::ToolResult, &result, 30),
        Some(vec!["result 42 full partial".to_string()])
    );
    // Another tool: the resolver declines, so the built-in view is used.
    assert_eq!(
        host.render(
            RenderKind::ToolCall,
            &json!({"toolName": "other", "args": {}}),
            30
        ),
        None
    );
    assert!(host.state().take_errors().is_empty());
}

/// The tool definition's own `renderCall` / `renderResult` are used when no resolver answers.
#[test]
fn tool_definition_renderers_are_the_fallback() {
    let (_env, host) = host_with(
        r#"oxa.registerTool({
            name: "shout",
            description: "shout a word",
            parameters: {type: "object", properties: {w: {type: "string"}}},
            execute: async (id, params) => ({content: params.w.toUpperCase()}),
            renderCall: (args) => ["shout? " + args.w],
            renderResult: (result, opts) => ["said " + result + (opts.isError ? " (error)" : "")],
        });"#,
    );
    assert_eq!(
        host.render(
            RenderKind::ToolCall,
            &json!({"toolName": "shout", "args": {"w": "hey"}}),
            20
        ),
        Some(vec!["shout? hey".to_string()])
    );
    assert_eq!(
        host.render(
            RenderKind::ToolResult,
            &json!({"toolName": "shout", "args": {}, "result": "HEY", "isError": true}),
            20
        ),
        Some(vec!["said HEY (error)".to_string()])
    );
}

/// only listed types
#[test]
fn markdown_transformer_message_types() {
    let (_env, host) = host_with(
        r#"oxa.registerMarkdownTransformer((md, ctx) => md.replace("cat", "dog"), {messageTypes: "user, assistant"});
           oxa.registerMarkdownTransformer((md) => md + "\n\n_footer_");"#,
    );
    // Listed type: both transformers run, in registration order.
    let user = json!({"messageType": "user", "markdown": "a cat"});
    assert_eq!(
        host.render(RenderKind::Markdown, &user, 80),
        Some(vec!["a dog".to_string(), "".to_string(), "_footer_".to_string()])
    );
    // Not listed: only the transformer without `messageTypes` (all types) applies.
    let custom = json!({"messageType": "custom", "markdown": "a cat"});
    assert_eq!(
        host.render(RenderKind::Markdown, &custom, 80),
        Some(vec!["a cat".to_string(), "".to_string(), "_footer_".to_string()])
    );
    assert!(host.state().take_errors().is_empty());
}

/// built-in view used
#[test]
fn no_renderer_returns_none() {
    let (_env, host) = host_with(
        r#"oxa.registerMessageRenderer("declines", () => undefined);
           oxa.registerMessageRenderer("throws", () => { throw new Error("boom"); });
           oxa.registerMarkdownTransformer((md) => { throw new Error("md boom"); }, {messageTypes: "user"});"#,
    );
    // Nothing registered for the type.
    assert_eq!(
        host.render(RenderKind::Message, &json!({"customType": "none"}), 10),
        None
    );
    assert_eq!(
        host.render(RenderKind::ToolCall, &json!({"toolName": "t", "args": {}}), 10),
        None
    );
    assert_eq!(
        host.render(
            RenderKind::Markdown,
            &json!({"messageType": "custom", "markdown": "x"}),
            10
        ),
        None
    );
    assert!(host.state().take_errors().is_empty());

    // A renderer that declines (undefined) gives the built-in view, without an error.
    assert_eq!(
        host.render(RenderKind::Message, &json!({"customType": "declines"}), 10),
        None
    );
    assert!(host.state().take_errors().is_empty());

    // A throwing renderer gives the built-in view and is reported.
    assert_eq!(
        host.render(RenderKind::Message, &json!({"customType": "throws"}), 10),
        None
    );
    let errors = host.state().take_errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].event, "render:message");
    assert!(errors[0].error.contains("boom"), "{errors:?}");

    // A throwing Markdown transformer leaves the text unchanged and is reported.
    let md = host.render(
        RenderKind::Markdown,
        &json!({"messageType": "user", "markdown": "keep me"}),
        10,
    );
    assert_eq!(md, Some(vec!["keep me".to_string()]));
    let errors = host.state().take_errors();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].event, "render:markdown");
}
