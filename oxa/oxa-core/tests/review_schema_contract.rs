use oxa_core::output_schema::OutputSchema;
use serde_json::{json, Value};

fn schema() -> OutputSchema {
    OutputSchema::new(
        serde_json::from_str(include_str!(
            "../../oxa-resources/builtin-skills/review/findings.schema.json"
        ))
        .unwrap(),
    )
    .expect("the shipped review schema must be supported by the runtime")
}

fn populated() -> Value {
    json!({"findings":[{
        "title":"[P2] Preserve the input", "body":"The input is lost on this branch.",
        "priority":2, "confidence":0.9, "path":"src/example.rs",
        "line_start":1, "line_end":2
    }], "overall_verdict":"incorrect"})
}

#[test]
fn review_schema_accepts_valid_outputs() {
    let schema = schema();
    for value in [json!({"findings":[],"overall_verdict":"correct"}), populated()] {
        assert!(schema.matches(&value.to_string()));
    }
    let mut value = populated();
    value["findings"][0]["priority"] = json!(2.0);
    assert!(
        schema.matches(&value.to_string()),
        "integer is a numeric value, not a JSON representation"
    );
}

#[test]
fn review_schema_rejects_malformed_findings() {
    let schema = schema();
    for (field, invalid) in [
        ("priority", json!(-1)),
        ("priority", json!(4)),
        ("priority", json!(1.5)),
        ("line_start", json!(0)),
        ("line_start", json!("1")),
        ("line_end", json!(0)),
        ("line_end", json!(1.5)),
        ("confidence", json!(-0.1)),
        ("confidence", json!(1.1)),
        ("title", json!("")),
        ("body", json!("")),
        ("path", json!("")),
        ("unexpected", json!(true)),
    ] {
        let mut value = populated();
        value["findings"][0][field] = invalid;
        assert!(!schema.matches(&value.to_string()), "accepted invalid {field}");
    }
    for value in [
        json!({"findings":[],"overall_verdict":"unknown"}),
        json!({"findings":[],"overall_verdict":"correct","extra":1}),
        json!({"findings":[]}),
    ] {
        assert!(!schema.matches(&value.to_string()));
    }
}

#[test]
fn review_schema_diagnostics_identify_instance_paths() {
    let schema = schema();
    for (field, invalid) in [
        ("priority", json!(4)),
        ("line_start", json!("1")),
        ("line_end", json!(0)),
    ] {
        let mut value = populated();
        value["findings"][0][field] = invalid;
        let error = schema.validate(&value.to_string()).unwrap_err();
        assert_eq!(error.instance_path, format!("/findings/0/{field}"));
        assert!(error.to_string().contains(&error.instance_path));
    }
    let error = schema
        .validate(r#"{"findings":[],"overall_verdict":"unknown"}"#)
        .unwrap_err();
    assert_eq!(error.instance_path, "/overall_verdict");
}

#[test]
fn review_subset_remains_offline_and_fail_closed() {
    for value in [
        json!({"$ref":"https://example.invalid/schema"}),
        json!({"pattern":"x"}),
        json!({"minimum":"1"}),
        json!({"minLength":-1}),
        json!({"additionalProperties":{}}),
        json!({"description":1}),
        json!({"properties":{"nested":{"unknown":true}}}),
    ] {
        assert!(OutputSchema::new(value).is_err());
    }
}

#[test]
fn review_subset_constraint_semantics_and_paths() {
    let schema = OutputSchema::new(json!({"properties":{"a/b~c":{"type":"string","minLength":2}}})).unwrap();
    assert!(schema.matches(r#"{"a/b~c":"é🦀"}"#));
    let error = schema.validate(r#"{"a/b~c":"🦀"}"#).unwrap_err();
    assert_eq!(error.instance_path, "/a~1b~0c");
    let schema = OutputSchema::new(json!({"required":["a/b~c"]})).unwrap();
    assert_eq!(schema.validate("{}").unwrap_err().instance_path, "/a~1b~0c");
    let schema = OutputSchema::new(json!({"additionalProperties":false})).unwrap();
    assert_eq!(
        schema.validate(r#"{"extra":0}"#).unwrap_err().instance_path,
        "/extra"
    );
    assert!(schema.validate("not JSON").unwrap_err().instance_path.is_empty());
    // Keywords constrain only their applicable instance types.
    assert!(OutputSchema::new(json!({"minimum":1,"minLength":2}))
        .unwrap()
        .matches("[]"));
    for (bound, valid, invalid) in [
        (
            json!({"minimum":9007199254740993_u64}),
            "9007199254740993",
            "9007199254740992.0",
        ),
        (
            json!({"maximum":9007199254740992.0}),
            "9007199254740992",
            "9007199254740993",
        ),
        (json!({"minimum":-1.5}), "-1", "-2"),
        (json!({"maximum":1.5}), "1", "2"),
    ] {
        let schema = OutputSchema::new(bound).unwrap();
        assert!(schema.matches(valid));
        assert!(!schema.matches(invalid));
    }
}

mod main_contract {

    use oxa_core::output_schema::OutputSchema;
    use serde_json::{json, Value};

    fn shipped_schema() -> OutputSchema {
        let value = serde_json::from_str(include_str!(
            "../../oxa-resources/builtin-skills/review/findings.schema.json"
        ))
        .expect("the shipped review schema must be JSON");
        OutputSchema::new(value).expect("the shipped review schema must use the supported runtime subset")
    }

    fn sample_finding() -> Value {
        json!({
            "findings": [{
                "title": "[P1] Preserve the final byte",
                "body": "The exclusive end drops the final byte of every nonempty buffer.",
                "priority": 1,
                "confidence": 0.98,
                "path": "src/buffer.rs",
                "line_start": 12,
                "line_end": 14
            }],
            "overall_verdict": "incorrect"
        })
    }

    #[test]
    fn review_schema_uses_supported_subset() {
        shipped_schema();
    }

    #[test]
    fn review_schema_accepts_two_sample_outputs() {
        let schema = shipped_schema();
        for sample in [
            json!({"findings": [], "overall_verdict": "correct"}),
            sample_finding(),
        ] {
            assert!(schema.matches(&sample.to_string()), "rejected {sample}");
        }
    }

    #[test]
    fn review_schema_enforces_finding_constraints() {
        let schema = shipped_schema();
        for (field, invalid) in [
            ("title", json!("")),
            ("body", json!("")),
            ("path", json!("")),
            ("priority", json!(-1)),
            ("priority", json!(4)),
            ("priority", json!(1.5)),
            ("confidence", json!(-0.01)),
            ("confidence", json!(1.01)),
            ("line_start", json!(0)),
            ("line_start", json!(1.5)),
            ("line_end", json!(0)),
            ("line_end", json!(1.5)),
            ("extra", json!(true)),
        ] {
            let mut sample = sample_finding();
            sample["findings"][0][field] = invalid;
            assert!(!schema.matches(&sample.to_string()), "accepted {sample}");
        }
        let mut extra_root = sample_finding();
        extra_root["extra"] = json!(true);
        assert!(!schema.matches(&extra_root.to_string()));
    }

    #[test]
    fn review_schema_rejects_three_malformed_outputs() {
        let schema = shipped_schema();
        let mut missing_body = sample_finding();
        missing_body["findings"][0]
            .as_object_mut()
            .unwrap()
            .remove("body");
        let mut wrong_line_type = sample_finding();
        wrong_line_type["findings"][0]["line_start"] = json!("twelve");
        let mut wrong_verdict = sample_finding();
        wrong_verdict["overall_verdict"] = json!("maybe");

        // These pointers identify the intended defects for assertion failures. They
        // are NOT validator diagnostics; testing returned paths needs a new API.
        for (sample, expected_path) in [
            (missing_body, "/findings/0/body"),
            (wrong_line_type, "/findings/0/line_start"),
            (wrong_verdict, "/overall_verdict"),
        ] {
            assert!(
                !schema.matches(&sample.to_string()),
                "accepted malformed output with a defect at {expected_path}: {sample}"
            );
        }
    }
}

#[test]
fn shipped_rejects_fractional_priority_and_confidence_over_one() {
    let schema = OutputSchema::new(
        serde_json::from_str(include_str!(
            "../../oxa-resources/builtin-skills/review/findings.schema.json"
        ))
        .unwrap(),
    )
    .unwrap();
    for (priority, confidence) in [("1.0000000000000001", "0.5"), ("1", "1.0000000000000001")] {
        let text = format!(
            r#"{{"findings":[{{"title":"x","body":"x","priority":{priority},"confidence":{confidence},"path":"src/example.rs","line_start":1,"line_end":1}}],"overall_verdict":"incorrect"}}"#
        );
        assert!(!schema.matches(&text), "accepted {text}");
    }
}
#[test]
fn confidence_over_one_is_rejected() {
    let schema = OutputSchema::new(json!({"type":"number","maximum":1})).unwrap();
    assert!(!schema.matches("1.0000000000000001"));
}
#[test]
fn numeric_bound_does_not_panic_on_large_json_number() {
    let schema = OutputSchema::new(json!({"type":"number","minimum":0})).unwrap();
    assert!(schema.matches("1e400"));
}

#[test]
fn lossless_numbers_preserve_syntax_enum_and_extreme_exponents() {
    let number = OutputSchema::new(json!({"type":"number","minimum":0,"maximum":1})).unwrap();
    for text in [
        "0",
        "-0",
        "0.000000000000000000000001",
        "1.0000000000000000",
        "1e-999999999999999999999999999999999999999999999",
    ] {
        assert!(number.matches(text), "{text}");
    }
    for text in [
        "-0.000000000000000000000001",
        "1.0000000000000001",
        "1e99999999999999999999999999999999999999999999",
        "01",
        "1.",
        "1e",
        "--1",
        "+1",
        "1-1",
        "NaN",
    ] {
        assert!(!number.matches(text), "{text}");
    }
    let integer = OutputSchema::new(json!({"type":"integer"})).unwrap();
    for text in ["1e400", "10e-1", "100.00e-2", "0e-9999999999999999999999999"] {
        assert!(integer.matches(text), "{text}");
    }
    for text in ["1e-400", "1.0000000000000001", "1e-9999999999999999999999999"] {
        assert!(!integer.matches(text), "{text}");
    }
    let nested = OutputSchema::new(json!({"enum":[{"n":[1,2]}]})).unwrap();
    assert!(nested.matches(r#"{"n":[1.00,20e-1]}"#));
    assert!(!nested.matches(r#"{"n":[1.0000000000000001,2]}"#));
    assert!(!nested.matches(r#"{"n":["1",2]}"#));
    let string = OutputSchema::new(json!({"enum":["1e400\\\"1"]})).unwrap();
    assert!(string.matches(&json!("1e400\\\"1").to_string()));
}

#[test]
fn four_hundred_digit_integers_and_large_exponents_validate_without_panics() {
    let positive = "9".repeat(400);
    let negative = format!("-{positive}");
    let schema = OutputSchema::new(json!({
        "type":"object", "properties":{
            "line":{"type":"integer","minimum":1},
            "confidence":{"type":"number","maximum":1}
        }
    }))
    .unwrap();
    for number in [&positive, "1e400"] {
        assert!(schema.validate(&format!(r#"{{"line":{number}}}"#)).is_ok());
        let error = schema
            .validate(&format!(r#"{{"confidence":{number}}}"#))
            .unwrap_err();
        assert_eq!(error.instance_path, "/confidence");
        assert_eq!(error.message, "Number is above maximum");
    }
    let error = schema.validate(&format!(r#"{{"line":{negative}}}"#)).unwrap_err();
    assert_eq!(error.instance_path, "/line");
    assert_eq!(error.message, "Number is below minimum");
}

#[test]
fn whole_number_boundaries_preserve_exact_decimal_semantics() {
    let schema = OutputSchema::new(json!({
        "type":"integer", "minimum":9007199254740993_u64, "maximum":9007199254740993_u64
    }))
    .unwrap();
    for number in [
        "9007199254740993",
        "9007199254740993.000",
        "900719925474099300e-2",
    ] {
        assert!(schema.validate(number).is_ok(), "{number}");
    }
    for number in [
        "9007199254740992.9999999999999999",
        "9007199254740993.0000000000000001",
        "9007199254740992",
        "9007199254740994",
    ] {
        assert!(schema.validate(number).is_err(), "{number}");
    }
}

#[test]
fn parser_private_number_maps_cannot_panic_numeric_checks() {
    let schema = OutputSchema::new(json!({"type":"number","minimum":0})).unwrap();
    // This is an object, not a JSON number. Some serde_json feature sets decode
    // its private representation as a Number; it must never bypass our view check.
    assert!(schema
        .validate(r#"{"$serde_json::private::Number":"1e400"}"#)
        .is_err());
}
