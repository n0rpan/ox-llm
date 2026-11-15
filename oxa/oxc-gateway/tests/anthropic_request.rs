#![cfg(feature = "upstream-anthropic-request")]
use oxc_gateway::anthropic_request::translate;
use serde_json::Value;

fn canonical(mut value: Value) -> Vec<u8> {
    value.sort_all_objects();
    serde_json::to_vec(&value).unwrap()
}

// The controller facade and shared crate consume the same source-captured cases.
#[test]
fn shared_library_matches_the_python_provider_requests() {
    for row in include_str!("../../../oxc-rs/tests/fixtures/anthropic_requests.jsonl").lines() {
        let row: Value = serde_json::from_str(row).unwrap();
        let result = translate(
            &row["input"],
            row["item"]["upstream_model"].as_str().unwrap(),
            row["item"]["max_output"].as_u64(),
        );
        if let Some(param) = row["error_param"].as_str() {
            assert_eq!(result.unwrap_err().param, param, "{}", row["name"]);
        } else {
            assert_eq!(
                canonical(result.unwrap()),
                canonical(row["expected"].clone()),
                "{}",
                row["name"]
            );
        }
    }
}
