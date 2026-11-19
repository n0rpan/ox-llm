//! B2a: the image content part on the wire. Images are never text, and a message without
//! images keeps its bytes.
use oxa_proto::*;

fn tiny_png() -> WireImage {
    WireImage {
        mime: "image/png".into(),
        base64: "QUJD".into(),
    }
}

#[test]
fn image_part_is_an_image_url_part_on_the_wire() {
    let mut m = WireMsg::user("hi");
    m.images.push(tiny_png());
    assert_eq!(
        String::from_utf8(m.to_json()).unwrap(),
        r#"{"role":"user","content":[{"type":"text","text":"hi"},{"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"}}]}"#
    );
}

#[test]
fn image_only_tool_result_has_no_text_part() {
    let mut m = WireMsg::tool_result("call_1", "");
    m.content = None;
    m.images.push(tiny_png());
    assert_eq!(
        String::from_utf8(m.to_json()).unwrap(),
        r#"{"role":"tool","tool_call_id":"call_1","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"}}]}"#
    );
}

#[test]
fn image_part_is_not_counted_as_text_tokens() {
    let mut m = WireMsg::user("look at this");
    let text_only = m.to_json().len();
    m.images.push(WireImage {
        mime: "image/png".into(),
        base64: "A".repeat(200_000),
    });
    assert_eq!(m.text_json_len(), text_only);
    assert!(m.to_json().len() > text_only + 200_000);
}

#[test]
fn text_json_len_equals_to_json_without_images() {
    let m = WireMsg::user("plain \"text\"");
    assert_eq!(m.text_json_len(), m.to_json().len());
}

#[test]
fn tool_output_starts_without_images() {
    assert!(ToolOutput::ok("x").images.is_empty());
    assert!(ToolOutput::error("x").images.is_empty());
}
