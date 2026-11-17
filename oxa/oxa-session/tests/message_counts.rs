//! Saved message counts used by the status view: user/assistant messages and the tool calls
//! their assistant messages carry.
use oxa_proto::{Entry, EntryBody, Role, ToolCall, WireMsg};
use oxa_session::message_counts;

fn entry(id: &str, wire: WireMsg) -> Entry {
    Entry {
        id: id.to_string(),
        parent: None,
        ts_ms: 1_750_000_000_000,
        body: EntryBody::Msg {
            wire,
            parts: Vec::new(),
            tool: None,
        },
    }
}

fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        args_raw: "{}".to_string(),
    }
}

#[test]
fn counts_user_assistant_messages_and_tool_calls() {
    let mut assistant_with_calls = WireMsg::assistant("");
    assistant_with_calls.tool_calls = vec![call("c1", "bash"), call("c2", "read")];
    let entries = vec![
        entry("a", WireMsg::user("first")),
        entry("b", assistant_with_calls),
        entry("c", WireMsg::tool_result("c1", "ok")),
        entry("d", WireMsg::tool_result("c2", "ok")),
        entry("e", WireMsg::user("second")),
        entry("f", WireMsg::assistant("done")),
        Entry {
            id: "g".to_string(),
            parent: None,
            ts_ms: 1_750_000_000_001,
            body: EntryBody::SessionInfo {
                name: "named".to_string(),
            },
        },
    ];
    let counts = message_counts(&entries);
    assert_eq!(counts.user, 2);
    assert_eq!(counts.assistant, 2);
    assert_eq!(counts.tool_calls, 2, "tool results are not calls");
    assert_eq!(
        entries
            .iter()
            .filter(|e| matches!(&e.body, EntryBody::Msg { wire, .. } if wire.role != Role::Tool))
            .count(),
        (counts.user + counts.assistant) as usize
    );
}

#[test]
fn an_empty_session_counts_nothing() {
    assert_eq!(message_counts(&[]), Default::default());
}
