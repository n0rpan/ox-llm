use oxa_proto::control::{ControlLine, ControlSubject, ControlVerb, MAX_CONTROL_LINE_BYTES};

#[test]
fn every_closed_verb_round_trips_without_changing_the_line() {
    for verb in [
        "ACK", "REQ", "Q", "ANS", "PROP", "VOTE(+1)", "VOTE(-1)", "VOTE(0)", "DECIDED", "BLESSED", "RETHINK",
        "APPROVED", "MERGED", "HOLD", "BLOCKED", "DONE",
    ] {
        let input = format!("chief> ox#123@eab40a5 {verb} m42/m41 concise reason");
        let line = ControlLine::parse(&input).unwrap();
        assert_eq!(line.to_string(), input);
        assert_eq!(line.sender(), "chief");
        assert_eq!(
            line.subject(),
            ControlSubject::Code {
                number: 123,
                commit: "eab40a5"
            }
        );
        assert_eq!(line.id(), "m42");
        assert_eq!(line.reply_to(), Some("m41"));
        assert_eq!(line.text(), "concise reason");
    }
}

#[test]
fn verdicts_decisions_and_holds_cannot_float_free_of_a_commit() {
    for verb in [
        "APPROVED", "DECIDED", "BLESSED", "MERGED", "HOLD", "RETHINK", "VOTE(+1)", "VOTE(-1)", "VOTE(0)",
    ] {
        for subject in ["~release", "ox#123", "ox#123@", "ox#123@eab40a"] {
            let input = format!("chief> {subject} {verb} m1 reason");
            assert!(ControlLine::parse(&input).is_err(), "{input}");
        }
    }
    let line = ControlLine::parse("worker> ~protocol PROP m1 add a verb").unwrap();
    assert_eq!(line.subject(), ControlSubject::Topic("protocol"));
    assert_eq!(line.verb(), ControlVerb::Propose);
}

#[test]
fn reasons_and_replies_are_structural_requirements() {
    for verb in ["HOLD", "BLOCKED", "RETHINK", "VOTE(-1)"] {
        assert!(ControlLine::parse(&format!("worker> ox#123@eab40a5 {verb} m1")).is_err());
    }
    for verb in ["ACK", "ANS"] {
        assert!(ControlLine::parse(&format!("worker> ~protocol {verb} m2")).is_err());
        assert!(ControlLine::parse(&format!("worker> ~protocol {verb} m2/m1")).is_ok());
    }
}

#[test]
fn malformed_or_prose_lines_are_rejected_instead_of_guessed() {
    for input in [
        "looks good, approved",
        "chief> ox#123@eab40a5 APPROVED",
        "chief>> ox#123@eab40a5 APPROVED m1",
        " chief> ox#123@eab40a5 APPROVED m1",
        "chief>  ox#123@eab40a5 APPROVED m1",
        "chief> ox#123@eab40a5 APPROVED m1 ",
        "chief> ox#123@eab40a5 approve m1",
        "chief> ox#123@eab40a5 VOTE(+2) m1",
        "chief> ox#123@eab40a5 APPROVED m1/",
        "chief> ox#123@eab40a5 APPROVED m1/m1",
        "chief> ox#123@eab40a5 APPROVED m1/m2/m3",
        "chief> ox#0@eab40a5 APPROVED m1",
        "chief> ox#01@eab40a5 APPROVED m1",
        "chief> ox#18446744073709551616@eab40a5 APPROVED m1",
        "chief> ox#123@EAB40A5 APPROVED m1",
        "chief> ox#123@zab40a5 APPROVED m1",
        "chief> ~ PROP m1",
        "chief> ~topic/name PROP m1",
        "chief> ox#123@eab40a5 HOLD m1 a\nsecond line",
        "chief> ox#123@eab40a5 HOLD m1 a\rline",
        "chief> ox#123@eab40a5 HOLD m1 a\tword",
        "chief> ox#123@eab40a5 HOLD m1 a\u{2028}line",
        "chief> ox#123@eab40a5 HOLD m1 a\u{2029}line",
        "chief> ox#123@eab40a5 HOLD m1  reason",
    ] {
        assert!(ControlLine::parse(input).is_err(), "{input:?}");
    }
}

#[test]
fn bounds_and_borrowed_unicode_reasons_are_preserved() {
    let prefix = "chief> ox#123@eab40a5 HOLD m1 ";
    let at_limit = format!("{prefix}{}", "x".repeat(MAX_CONTROL_LINE_BYTES - prefix.len()));
    assert_eq!(ControlLine::parse(&at_limit).unwrap().to_string(), at_limit);
    assert!(ControlLine::parse(&(at_limit + "x")).is_err());
    assert!(ControlLine::parse(&format!("chief> ~topic PROP {}", "a".repeat(65))).is_err());
    let input = "worker> ox#123@eab40a5 HOLD m1 α reason with  two spaces";
    let line = ControlLine::parse(input).unwrap();
    assert_eq!(line.text(), "α reason with  two spaces");
    assert_eq!(line.to_string(), input);
    assert!(
        line.text().as_ptr() >= input.as_ptr()
            && line.text().as_ptr() < input.as_ptr().wrapping_add(input.len())
    );
}
