//! Plan item 42: the shipped review schema must work with the runtime validator.
//!
//! This is deliberately not a projected or simplified schema: accepting a copy
//! with unsupported constraints removed would hide the shipping incompatibility.
//! `OutputSchema::matches` returns only a bool, not an instance error path.

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
