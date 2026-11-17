use oxa_proto::{
    wire_is_prefix, write_wire_json, ProviderHistory, ProviderHistoryError as Error, WireMsg,
    MAX_PROVIDER_HISTORY_BYTES, MAX_PROVIDER_HISTORY_ITEMS,
};
use serde_json::{json, Value};

fn history() -> ProviderHistory {
    ProviderHistory::new(
        [0xab; 32],
        [0xcd; 32],
        vec![
            json!({"type":"reasoning","encrypted_content":"synthetic-reasoning-blob"}),
            json!({"type":"function_call","call_id":"synthetic-call","name":"read",
                "arguments":"{ \"path\" : \"a.rs\" }"}),
        ],
    )
    .unwrap()
}

#[test]
fn private_round_trip_keeps_item_order_exact_arguments_and_bindings() {
    let original = history();
    let stored = original.to_session_value();
    assert_eq!(stored["v"], 1);
    assert_eq!(stored["scope"], "ab".repeat(32));
    assert_eq!(stored["prefix"], "cd".repeat(32));
    let decoded = ProviderHistory::from_session_value(&stored).unwrap();
    assert_eq!(decoded, original);
    assert_eq!(decoded.scope(), &[0xab; 32]);
    assert_eq!(decoded.prefix(), &[0xcd; 32]);
    assert_eq!(decoded.items()[0]["type"], "reasoning");
    assert_eq!(decoded.items()[1]["arguments"], "{ \"path\" : \"a.rs\" }");
    assert_eq!(format!("{original:?}"), "ProviderHistory([redacted])");
}

#[test]
fn visible_wire_omits_carrier_and_prefix_guard_rejects_replacement_authority() {
    let plain = WireMsg::assistant("visible reply");
    let mut private = plain.clone();
    private.provider_history = Some(history());
    assert_eq!(private.to_json(), plain.to_json());
    let mut wire = vec![];
    write_wire_json(std::slice::from_ref(&private), &mut wire);
    assert_eq!(wire, br#"[{"role":"assistant","content":"visible reply"}]"#);
    assert!(wire_is_prefix(
        std::slice::from_ref(&private),
        &[private.clone(), WireMsg::user("next")]
    ));
    // A hook rebuilding arbitrary visible JSON cannot replace replay authority.
    assert!(!wire_is_prefix(std::slice::from_ref(&private), &[plain]));
    let mut replaced = private.clone();
    replaced.provider_history = Some(ProviderHistory::new([1; 32], [2; 32], vec![]).unwrap());
    assert!(!wire_is_prefix(&[private], &[replaced]));
}

#[test]
fn present_bad_or_unknown_session_state_is_an_explicit_error() {
    let stored = history().to_session_value();
    for value in [Value::Null, json!([]), json!({}), json!({"v":"1"})] {
        assert_eq!(ProviderHistory::from_session_value(&value), Err(Error::Malformed));
    }
    for version in [0, 2, u64::MAX] {
        let mut value = stored.clone();
        value["v"] = json!(version);
        assert_eq!(
            ProviderHistory::from_session_value(&value),
            Err(Error::UnsupportedVersion)
        );
    }
    for field in ["scope", "prefix"] {
        for bad in [
            json!(null),
            json!("ab"),
            json!("AB".repeat(32)),
            json!("gg".repeat(32)),
        ] {
            let mut value = stored.clone();
            value[field] = bad;
            assert_eq!(ProviderHistory::from_session_value(&value), Err(Error::Malformed));
        }
    }
    for bad in [json!(null), json!({}), json!([null]), json!(["synthetic"])] {
        let mut value = stored.clone();
        value["items"] = bad;
        assert_eq!(ProviderHistory::from_session_value(&value), Err(Error::Malformed));
    }
    let mut extra = stored;
    extra["extra"] = json!(true);
    assert_eq!(ProviderHistory::from_session_value(&extra), Err(Error::Malformed));
}

#[test]
fn item_and_encoded_byte_limits_apply_at_construction_and_session_load() {
    let maximum = vec![json!({}); MAX_PROVIDER_HISTORY_ITEMS];
    ProviderHistory::new([0; 32], [0; 32], maximum.clone()).unwrap();
    let mut too_many = maximum;
    too_many.push(json!({}));
    assert_eq!(
        ProviderHistory::new([0; 32], [0; 32], too_many.clone()),
        Err(Error::TooManyItems)
    );
    let mut stored = history().to_session_value();
    stored["items"] = json!(too_many);
    assert_eq!(
        ProviderHistory::from_session_value(&stored),
        Err(Error::TooManyItems)
    );

    let overhead = serde_json::to_vec(&json!([{"text":""}])).unwrap().len();
    let at_limit = json!({"text":"x".repeat(MAX_PROVIDER_HISTORY_BYTES - overhead)});
    let bounded = ProviderHistory::new([0; 32], [0; 32], vec![at_limit]).unwrap();
    assert_eq!(
        serde_json::to_vec(bounded.items()).unwrap().len(),
        MAX_PROVIDER_HISTORY_BYTES
    );
    ProviderHistory::from_session_value(&bounded.to_session_value()).unwrap();
    let mut over = bounded.to_session_value();
    // Escaped newline and multi-byte UTF-8 both count by encoded bytes.
    over["items"][0]["text"] = json!(format!(
        "{}\nλ",
        "x".repeat(MAX_PROVIDER_HISTORY_BYTES - overhead)
    ));
    assert_eq!(ProviderHistory::from_session_value(&over), Err(Error::TooLarge));
    let items = over["items"].as_array().unwrap().clone();
    assert_eq!(
        ProviderHistory::new([0; 32], [0; 32], items),
        Err(Error::TooLarge)
    );
}
