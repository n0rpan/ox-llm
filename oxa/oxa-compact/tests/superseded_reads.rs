//! #459: a kept tail drops the result of an older `read` of a file that the tail reads again
//! (same page) or edits/writes later; the newest read and other pages stay verbatim.
use oxa_compact::{prompt, tail_elisions};
use oxa_proto::{Role, ToolCall, WireMsg};

fn count(s: &str) -> u32 {
    s.chars().count().div_ceil(4) as u32
}

fn call(id: &str, name: &str, args: &str) -> WireMsg {
    WireMsg {
        role: Role::Assistant,
        content: None,
        reasoning: None,
        tool_calls: vec![ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            args_raw: args.to_owned(),
        }],
        tool_call_id: None,
    }
}

#[test]
fn older_read_of_a_reread_or_edited_file_is_dropped() {
    let tail = vec![
        WireMsg::user("go"),
        call("c1", "read", r#"{"path":"src/a.rs"}"#),
        WireMsg::tool_result("c1", "old a"),
        call("c2", "read", r#"{"path":"src/b.rs"}"#),
        WireMsg::tool_result("c2", "old b"),
        call("c3", "read", r#"{"path":"src/c.rs","offset":1}"#),
        WireMsg::tool_result("c3", "page 1 of c"),
        call("c4", "read", r#"{"path":"src/a.rs"}"#),
        WireMsg::tool_result("c4", "new a"),
        call("c5", "edit", r#"{"path":"src/b.rs","edits":[]}"#),
        WireMsg::tool_result("c5", "edited"),
        call("c6", "read", r#"{"path":"src/c.rs","offset":400}"#),
        WireMsg::tool_result("c6", "page 2 of c"),
    ];
    let got = tail_elisions(&tail, 4000, &count);
    let idx: Vec<usize> = got.iter().map(|(k, _)| *k).collect();
    assert_eq!(idx, vec![2, 4], "{got:?}");
    assert_eq!(got[0].1, prompt::superseded_read("src/a.rs"));
}
