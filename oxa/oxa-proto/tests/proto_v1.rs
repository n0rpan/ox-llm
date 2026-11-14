//! Golden tests of the frozen wire bytes. If one of these changes, PROTO_VERSION must change.
use oxa_proto::*;

fn json(m: &WireMsg) -> String {
    String::from_utf8(m.to_json()).unwrap()
}

#[test]
fn user_and_system_bytes() {
    assert_eq!(
        json(&WireMsg::user("hi \"x\"\n")),
        r#"{"role":"user","content":"hi \"x\"\n"}"#
    );
    assert_eq!(json(&WireMsg::system("s")), r#"{"role":"system","content":"s"}"#);
}

#[test]
fn assistant_bytes_keep_everything_as_streamed() {
    let m = WireMsg {
        role: Role::Assistant,
        content: Some("  spaced  ".into()),
        reasoning: Some("think ".into()),
        tool_calls: vec![ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            args_raw: "{\"path\": \"a.rs\" }".into(),
        }],
        tool_call_id: None,
    };
    assert_eq!(
        json(&m),
        r#"{"role":"assistant","content":"  spaced  ","reasoning_content":"think ","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\": \"a.rs\" }"}}]}"#
    );
}

#[test]
fn assistant_without_content_is_null_and_omits_empty_fields() {
    let m = WireMsg {
        role: Role::Assistant,
        content: None,
        reasoning: None,
        tool_calls: vec![],
        tool_call_id: None,
    };
    assert_eq!(json(&m), r#"{"role":"assistant","content":null}"#);
    let e = WireMsg {
        reasoning: Some(String::new()),
        ..m
    };
    assert_eq!(
        json(&e),
        r#"{"role":"assistant","content":null,"reasoning_content":""}"#
    );
}

#[test]
fn tool_bytes_and_unicode_raw() {
    assert_eq!(
        json(&WireMsg::tool_result("call_1", "å 😀")),
        "{\"role\":\"tool\",\"tool_call_id\":\"call_1\",\"content\":\"å 😀\"}"
    );
}

#[test]
fn wire_array_and_prefix_relation() {
    let a = vec![WireMsg::user("1"), WireMsg::assistant("2")];
    let mut b = a.clone();
    b.push(WireMsg::user("3"));
    let mut out = Vec::new();
    write_wire_json(&a, &mut out);
    assert_eq!(
        out,
        br#"[{"role":"user","content":"1"},{"role":"assistant","content":"2"}]"#
    );
    assert!(wire_is_prefix(&a, &b));
    assert!(!wire_is_prefix(&b, &a));
    b[0] = WireMsg::user("x");
    assert!(!wire_is_prefix(&a, &b));
}

#[test]
fn key_ids_normalise() {
    assert_eq!(KeyId::parse("Shift+Ctrl+Up").unwrap().as_str(), "ctrl+shift+up");
    assert_eq!(KeyId::parse("alt+m").unwrap().as_str(), "alt+m");
    assert_eq!(KeyId::parse("meta+return").unwrap().as_str(), "alt+enter");
    assert_eq!(KeyId::parse("ctrl++").unwrap().as_str(), "ctrl++");
    assert_eq!(KeyId::parse("ctrl++").unwrap().key(), "+");
    assert_eq!(KeyId::parse("+").unwrap().key(), "+");
    assert!(KeyId::parse("ctrl+shift+up").unwrap().has_mod("shift"));
    assert!(!KeyId::parse("up").unwrap().has_mod("shift"));
    assert!(KeyId::parse("hyper+x").is_err());
    assert!(KeyId::parse("ctrl+").is_err());
    assert!(KeyId::parse("").is_err());
}

#[test]
fn names_round_trip() {
    for l in ThinkingLevel::ALL {
        assert_eq!(ThinkingLevel::parse(l.as_str()), Some(l));
    }
    for p in HookPoint::ALL {
        assert_eq!(HookPoint::parse(p.as_str()), Some(p));
    }
    for m in Mode::ALL {
        assert_eq!(Mode::parse(m.as_str()), Some(m));
    }
    assert!(!ThinkingLevel::Off.enabled() && ThinkingLevel::Minimal.enabled());
    assert_eq!(HookPoint::Context.guard(), GuardClass::PrefixChecked);
    assert_eq!(HookPoint::ToolCall.guard(), GuardClass::AppendSafe);
    assert_eq!(HookPoint::TurnEnd.guard(), GuardClass::Observer);
}

#[test]
fn prefix_text_joins_sections_and_headless_ui_denies() {
    let p = FrozenPrefix {
        sections: vec![
            ("preamble".into(), "A".into()),
            ("cwd".into(), "Current working directory: /w".into()),
        ],
        tools_json: "[]".into(),
        sha256: [0; 32],
    };
    assert_eq!(p.system_text(), "A\n\nCurrent working directory: /w");
    assert_eq!(p.hash_input(), b"A\n\nCurrent working directory: /w\0[]");
    let mut ui = HeadlessUi::default();
    assert_eq!(
        ui.ask_access(&Access::Net {
            host: "example.org".into()
        }),
        Answer::Deny
    );
    assert_eq!(ui.confirm("t", "b", None), None);
    assert_eq!(
        image_placeholder("a.png", 3, 4),
        "[image a.png 3x4: not visible to this model]"
    );
}
