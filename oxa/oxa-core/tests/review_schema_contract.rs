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
        json!({"$schema":"https://example.invalid/schema"}),
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
