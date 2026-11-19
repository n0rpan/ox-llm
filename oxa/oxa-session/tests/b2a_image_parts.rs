//! B2a: image parts survive a session file, and a session written before images still loads.
use oxa_proto::{Entry, EntryBody, WireImage, WireMsg};
use oxa_session::{decode_entry, encode_entry};

fn msg_entry(wire: WireMsg) -> Entry {
    Entry {
        id: "0000000b".into(),
        parent: Some("0000000a".into()),
        ts_ms: 1_733_234_400_000,
        body: EntryBody::Msg {
            wire,
            parts: vec![],
            tool: None,
        },
    }
}

#[test]
fn image_part_round_trips() {
    let mut wire = WireMsg::tool_result("call_1", "[image shot.png 2x2]");
    wire.images.push(WireImage {
        mime: "image/png".into(),
        base64: "iVBORw0KGgo=".into(),
    });
    let e = msg_entry(wire);
    let mut line = Vec::new();
    encode_entry(&e, &mut line);
    let back = decode_entry(&line).expect("an entry with an image decodes");
    assert_eq!(back, e);
}

#[test]
fn old_sessions_without_images_still_load() {
    let e = msg_entry(WireMsg::user("written before images existed"));
    let mut line = Vec::new();
    encode_entry(&e, &mut line);
    // The key is written only when images exist; the text itself mentions images.
    assert!(!String::from_utf8_lossy(&line).contains("\"images\""));
    assert_eq!(decode_entry(&line).expect("old entry decodes"), e);
}
