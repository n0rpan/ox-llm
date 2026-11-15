use oxa_engine::parse_info_for;
use serde_json::{json, Value};

#[test]
fn remote_catalog_limits_override_loaded_local_limits() {
    let models: Value = serde_json::from_str(include_str!("fixtures/remote-models.json")).unwrap();
    let status = json!({"model":"local", "context_size":131072, "max_output":16384,
        "model_limits":{"anthropic/claude-sonnet-5-5":{"cap":131072,"max_output":16384}}});
    let info = parse_info_for(
        "http://localhost:1234",
        &models,
        &status,
        Some("anthropic/claude-sonnet-5-5"),
    )
    .unwrap();
    assert_eq!(info.n_ctx, 200000);
    assert_eq!(info.max_output, 32000);
    assert_eq!(info.limits_for, "anthropic/claude-sonnet-5-5");
    assert_eq!(info.compact_at, None);
    let local = parse_info_for("http://localhost:1234", &models, &status, Some("local")).unwrap();
    assert_eq!((local.n_ctx, local.max_output), (131072, 16384));
}

#[test]
fn remote_loaded_model_uses_catalog_without_local_context() {
    let models: Value = serde_json::from_str(include_str!("fixtures/remote-models.json")).unwrap();
    let status = json!({"model":"anthropic/claude-sonnet-5-5"});
    let info = parse_info_for("http://localhost:1234", &models, &status, None).unwrap();
    assert_eq!((info.n_ctx, info.max_output), (200000, 32000));
}
