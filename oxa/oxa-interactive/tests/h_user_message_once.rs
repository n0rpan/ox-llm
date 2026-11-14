//! Wave H (lane run): the core reports a sent user message twice, as its stored entry
//! (`entry_appended`, with the image parts) and as pi's `message_start` / `message_end` at the
//! turn that sends it (json.md "Event sequence"). The transcript shows it once.

use oxa_interactive::{apply_event, Block, ViewModel};
use oxa_proto::{AgentEvent, Entry, EntryBody, EventMessage, Part, StopReason, WireMsg};

fn entry(id: &str, text: &str) -> AgentEvent {
    AgentEvent::EntryAppended {
        entry: Entry {
            id: id.into(),
            parent: None,
            ts_ms: 0,
            body: EntryBody::Msg {
                wire: WireMsg::user(text),
                parts: vec![Part::Text(text.into())],
            },
        },
    }
}

fn message(msg: WireMsg, stop: Option<StopReason>) -> EventMessage {
    EventMessage {
        msg,
        ts_ms: 0,
        stop,
        usage: None,
        model: None,
        tool_name: None,
        is_error: false,
        error_message: None,
    }
}

fn users(vm: &ViewModel) -> Vec<String> {
    vm.blocks
        .iter()
        .filter_map(|b| match b {
            Block::User { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn stored_and_announced_user_message_is_one_block() {
    let mut vm = ViewModel::default();
    // Two identical follow-ups stored before the turn, then announced at the turn start.
    for e in [
        entry("00000001", "ok"),
        entry("00000002", "ok"),
        AgentEvent::AgentStart,
        AgentEvent::TurnStart,
        AgentEvent::MessageStart {
            message: message(WireMsg::user("ok"), None),
        },
        AgentEvent::MessageEnd {
            message: message(WireMsg::user("ok"), None),
        },
        AgentEvent::MessageStart {
            message: message(WireMsg::user("ok"), None),
        },
        AgentEvent::MessageStart {
            message: message(WireMsg::assistant(""), Some(StopReason::Pending)),
        },
        AgentEvent::MessageEnd {
            message: message(WireMsg::assistant("fine"), Some(StopReason::Stop)),
        },
    ] {
        apply_event(&mut vm, &e);
    }
    assert_eq!(users(&vm), ["ok", "ok"]);

    // The same text again in the next turn is a new message.
    apply_event(&mut vm, &entry("00000003", "ok"));
    apply_event(
        &mut vm,
        &AgentEvent::MessageStart {
            message: message(WireMsg::user("ok"), None),
        },
    );
    assert_eq!(users(&vm), ["ok", "ok", "ok"]);

    // A user message_start with no stored entry (another producer) still adds a block.
    apply_event(
        &mut vm,
        &AgentEvent::MessageStart {
            message: message(WireMsg::assistant(""), Some(StopReason::Pending)),
        },
    );
    apply_event(
        &mut vm,
        &AgentEvent::MessageStart {
            message: message(WireMsg::user("from elsewhere"), None),
        },
    );
    assert_eq!(users(&vm), ["ok", "ok", "ok", "from elsewhere"]);
}
