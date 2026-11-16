use oxa_proto::{ChatRequest, FrozenPrefix, Hints};

#[test]
fn chat_request_new_fills_defaults_so_struct_update_survives_new_fields() {
    let prefix = FrozenPrefix {
        sections: vec![],
        tools_json: "[]".into(),
        sha256: [0; 32],
    };
    let r = ChatRequest {
        max_tokens: 7,
        ..ChatRequest::new(&prefix, &[], "m")
    };
    assert_eq!(
        (r.max_tokens, r.model, r.thinking, r.sampling.is_none()),
        (7, "m", false, true)
    );
    assert_eq!(r.hints, Hints::default());
    assert!(r.output_schema.is_none());
}
