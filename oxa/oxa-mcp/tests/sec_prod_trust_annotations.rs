//! Item 4 (sec-prod): `readOnlyHint` is the server's claim. A stdio server's hint is believed by
//! default; a remote (HTTP) server's is not, unless its entry says `"trustAnnotations": true`.
//! An explicit `false` turns the stdio trust off. The four combinations, through the parsed
//! config and the class the tool reports (the policy maps `Read` to allow in plan and workspace,
//! `Write` to deny in plan and to ask in workspace).
use oxa_mcp::{parse_server, tool_spec, ConfigScope, McpTool, ServerConfig, ServerKind, ToolAnnotations};
use oxa_proto::ToolClass;
use serde_json::json;

fn server(entry: serde_json::Value) -> ServerConfig {
    parse_server("srv", &entry, ConfigScope::User).expect("valid entry")
}

fn read_only_tool() -> McpTool {
    McpTool {
        name: "look".to_string(),
        description: "reads".to_string(),
        input_schema: json!({"type": "object", "properties": {}}),
        annotations: ToolAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        },
    }
}

fn class_of(cfg: &ServerConfig) -> ToolClass {
    tool_spec(cfg, &read_only_tool(), "mcp__srv__look").class
}

const STDIO: fn() -> serde_json::Value = || json!({"command": "server-bin"});
const HTTP: fn() -> serde_json::Value = || json!({"url": "https://mcp.example.com/mcp"});

#[test]
fn stdio_default_trusts_the_read_only_hint() {
    let cfg = server(STDIO());
    assert!(cfg.trust_annotations);
    assert_eq!(class_of(&cfg), ToolClass::Read);
}

#[test]
fn stdio_explicit_false_turns_the_trust_off() {
    let mut entry = STDIO();
    entry["trustAnnotations"] = json!(false);
    let cfg = server(entry);
    assert!(!cfg.trust_annotations);
    assert_eq!(class_of(&cfg), ToolClass::Write);
}

#[test]
fn remote_default_does_not_trust_the_read_only_hint() {
    let cfg = server(HTTP());
    assert!(matches!(cfg.kind, ServerKind::Http(_)));
    assert!(!cfg.trust_annotations);
    assert_eq!(class_of(&cfg), ToolClass::Write);
}

#[test]
fn remote_explicit_true_trusts_the_read_only_hint() {
    let mut entry = HTTP();
    entry["trustAnnotations"] = json!(true);
    let cfg = server(entry);
    assert!(cfg.trust_annotations);
    assert_eq!(class_of(&cfg), ToolClass::Read);
}

#[test]
fn a_non_boolean_value_is_an_invalid_entry() {
    let mut entry = HTTP();
    entry["trustAnnotations"] = json!("yes");
    let err = parse_server("srv", &entry, ConfigScope::User).expect_err("refused");
    assert_eq!(err, "server \"srv\": trustAnnotations must be true or false");
}

#[test]
fn an_untrusted_tool_without_the_hint_is_write_in_every_server_kind() {
    let mut tool = read_only_tool();
    tool.annotations = ToolAnnotations::default();
    for entry in [STDIO(), HTTP()] {
        let cfg = server(entry);
        assert_eq!(tool_spec(&cfg, &tool, "mcp__srv__look").class, ToolClass::Write);
    }
}
