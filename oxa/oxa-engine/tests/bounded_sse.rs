use oxa_engine::{SseEvent, SseParser};

#[test]
fn incomplete_lines_and_multiline_events_have_a_buffer_cap() {
    let mut parser = SseParser::new();
    assert!(parser
        .feed_bounded(b"data: 123456789012345678901234567890", 16)
        .is_err());
    let mut parser = SseParser::new();
    assert!(parser
        .feed_bounded(b"data: 1234\ndata: 1234\ndata: 1234\n", 24)
        .is_err());
}

#[test]
fn complete_events_release_capacity_and_all_byte_splits_preserve_utf8() {
    let input = "data: λ世界\r\n\r\ndata: next\n\ndata: [DONE]\n\n".as_bytes();
    let expected = vec![
        SseEvent::Data("λ世界".into()),
        SseEvent::Data("next".into()),
        SseEvent::Done,
    ];
    for split in 0..=input.len() {
        let mut parser = SseParser::new();
        let mut events = parser.feed_bounded(&input[..split], 32).unwrap();
        events.extend(parser.feed_bounded(&input[split..], 32).unwrap());
        assert_eq!(events, expected);
    }
    let mut parser = SseParser::new();
    for _ in 0..1000 {
        assert_eq!(
            parser.feed_bounded(b"data: ok\n\n", 16).unwrap(),
            vec![SseEvent::Data("ok".into())]
        );
    }
}
