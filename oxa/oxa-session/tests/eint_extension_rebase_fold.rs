//! The fold of a declared extension rebase entry (`Transcript::EXTENSION_REBASE_ENTRY`).

use oxa_proto::{rebase_why, Entry, EntryBody, ToolCall, WireMsg};
use oxa_session::{decode_entry, encode_entry, Transcript};
use serde_json::json;

fn entry(id: &str, body: EntryBody) -> Entry {
    Entry {
        id: id.into(),
        parent: None,
        ts_ms: 1,
        body,
    }
}

fn msg(id: &str, wire: WireMsg) -> Entry {
    entry(
        id,
        EntryBody::Msg {
            wire,
            parts: Vec::new(),
            tool: None,
        },
    )
}

fn assistant() -> WireMsg {
    WireMsg {
        role: oxa_proto::Role::Assistant,
        content: Some(String::new()),
        reasoning: Some("think".into()),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args_raw: "{ \"path\" :\"a\"}".into(),
        }],
        provider_history: None,
        tool_call_id: None,
    }
}

/// A session file written before the list was stored has only the marker: the fold ignores it.
#[test]
fn marker_without_messages_is_a_no_op() {
    let branch = vec![
        msg("e1", WireMsg::user("a")),
        entry(
            "e2",
            EntryBody::Custom {
                custom_type: Transcript::EXTENSION_REBASE_ENTRY.into(),
                data: json!({"extension": "x.js", "event": "context", "why": "extension", "before": 1, "after": 1}),
            },
        ),
    ];
    let t = Transcript::project(&branch);
    assert_eq!(t.wire(), &[WireMsg::user("a")]);
    assert!(t.rebases().is_empty());
    assert_eq!(t.origin(0), Some("e1"));
}

/// The stored list replaces the wire (cause `extension`), verbatim through the session codec;
/// identical messages at the same position keep their origin.
#[test]
fn stored_list_is_folded_as_a_rebase_through_the_codec() {
    let head = vec![WireMsg::user("a"), assistant(), WireMsg::user("CHANGED")];
    let rebase = entry(
        "e4",
        EntryBody::Custom {
            custom_type: Transcript::EXTENSION_REBASE_ENTRY.into(),
            data: json!({"extension": "x.js", "messages": Transcript::encode_rebase(&head)}),
        },
    );
    // Round trip the entry through the file codec.
    let mut line = Vec::new();
    encode_entry(&rebase, &mut line);
    let rebase = decode_entry(&line).expect("decode");
    let branch = vec![
        msg("e1", WireMsg::user("a")),
        msg("e2", assistant()),
        msg("e3", WireMsg::user("b")),
        rebase,
    ];
    let t = Transcript::project(&branch);
    assert_eq!(t.wire(), head.as_slice());
    assert_eq!(t.rebases().last().map(|r| r.why), Some(rebase_why::EXTENSION));
    assert_eq!(t.expected_cache(), 0);
    assert_eq!(
        (t.origin(0), t.origin(1), t.origin(2)),
        (Some("e1"), Some("e2"), None)
    );
    let mut inc = Transcript::default();
    for e in &branch {
        inc.apply(e);
    }
    assert_eq!(inc, t, "incremental fold = projection");
}
