use super::{checked_models, DiscoveryError as D};
use serde_json::{json, Value};

#[test]
fn catalog_diagnostics_preserve_each_validation_guard() {
    let base = "https://example.invalid/inference/go";
    let valid = json!({"data":[{"id":"synthetic", "canonical_id":"synthetic/canonical",
        "limit":{"context":100,"output":10},
        "api":{"chat-completions":{"base_url":format!("{base}/openai/v1")}}}]});
    assert_eq!(checked_models(&valid, base).unwrap().len(), 1);
    for (pointer, replacement, expected) in [
        ("/data", Value::Null, D::CatalogShape),
        ("/data/0/id", json!(""), D::ModelIdentifier),
        (
            "/data/0/canonical_id",
            json!("invalid identifier"),
            D::ModelIdentifier,
        ),
        ("/data/0/limit/context", json!(0), D::ModelLimits),
        ("/data/0/limit/output", json!(101), D::ModelLimits),
        ("/data/0/api", json!({}), D::ApiShape),
        ("/data/0/api/chat-completions/base_url", Value::Null, D::ApiShape),
        (
            "/data/0/api/chat-completions/base_url",
            json!("https://other.invalid/inference/go"),
            D::Endpoint,
        ),
        (
            "/data/0/api/chat-completions/base_url",
            json!("https://example.invalid/inference/paid"),
            D::Endpoint,
        ),
    ] {
        let mut value = valid.clone();
        *value.pointer_mut(pointer).unwrap() = replacement;
        assert_eq!(checked_models(&value, base).unwrap_err(), expected);
        assert!(expected.code().starts_with("cloud_discovery_"));
        assert!(expected.message().starts_with("Go discovery failed:"));
    }
    let mut duplicate = valid.clone();
    duplicate["data"]
        .as_array_mut()
        .unwrap()
        .push(valid["data"][0].clone());
    assert_eq!(checked_models(&duplicate, base).unwrap_err(), D::DuplicateModel);
    // Reject the entire catalog, including when a valid record comes first.
    let mut partial = valid.clone();
    partial["data"].as_array_mut().unwrap().push(json!({}));
    assert_eq!(checked_models(&partial, base).unwrap_err(), D::ModelLimits);
}
