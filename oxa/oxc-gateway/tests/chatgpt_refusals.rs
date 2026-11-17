#![cfg(all(feature = "anthropic-client", feature = "ledger"))]

use oxc_gateway::rate_limits::{chatgpt::Refusal, classify_chatgpt_refusal};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::json;
use std::time::Duration;

#[test]
fn chatgpt_refusals_parity_table() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", HeaderValue::from_static("7"));
    for (kind, expected) in [
        (
            "usage_limit_reached",
            Refusal::UsageLimitReached {
                plan_type: Some("synthetic-plan".into()),
                resets_at: Some(1_800_000_000),
                retry_after: Some(Duration::from_secs(7)),
            },
        ),
        ("usage_not_included", Refusal::UsageNotIncluded),
    ] {
        let body = serde_json::to_vec(&json!({"error": {
            "type": kind, "plan_type": "synthetic-plan", "resets_at": 1_800_000_000,
            "message": "ignored synthetic message"
        }}))
        .unwrap();
        assert_eq!(classify_chatgpt_refusal(429, &body, &headers), Some(expected));
        for status in [200, 400, 401, 403, 503] {
            assert_eq!(classify_chatgpt_refusal(status, &body, &headers), None);
        }
    }
}

#[test]
fn chatgpt_refusals_optional_fields_are_independent() {
    for (fields, plan_type, resets_at) in [
        (json!({}), None, None),
        (
            json!({"plan_type": "synthetic-plan"}),
            Some("synthetic-plan"),
            None,
        ),
        (json!({"resets_at": 0}), None, Some(0)),
        (json!({"plan_type": null, "resets_at": null}), None, None),
        (json!({"plan_type": 42, "resets_at": "1800000000"}), None, None),
        (json!({"plan_type": [], "resets_at": -1}), None, None),
        (json!({"plan_type": {}, "resets_at": 1.5}), None, None),
        (
            json!({"resets_at": 18446744073709551615u64}),
            None,
            Some(u64::MAX),
        ),
        (
            json!({"plan_type": "synthetic-plan", "resets_at": {}}),
            Some("synthetic-plan"),
            None,
        ),
    ] {
        let mut error = fields;
        error["type"] = json!("usage_limit_reached");
        let body = serde_json::to_vec(&json!({"error": error})).unwrap();
        assert_eq!(
            classify_chatgpt_refusal(429, &body, &HeaderMap::new()),
            Some(Refusal::UsageLimitReached {
                plan_type: plan_type.map(str::to_owned),
                resets_at,
                retry_after: None,
            })
        );
    }
}

#[test]
fn chatgpt_refusals_unknown_shapes_are_not_generic_rate_limits() {
    for body in [
        b"".as_slice(),
        b"not json",
        b"null",
        b"[]",
        b"{}",
        br#"{"error":null}"#,
        br#"{"error":[]}"#,
        br#"{"error":{"type":42}}"#,
        br#"{"error":{"type":"unknown"}}"#,
        br#"{"error":{"code":"usage_limit_reached"}}"#,
        br#"{"type":"usage_limit_reached"}"#,
        br#"{"error":{"type":"usage_limit_reached"}} trailing"#,
    ] {
        assert_eq!(classify_chatgpt_refusal(429, body, &HeaderMap::new()), None);
    }
}

#[test]
fn chatgpt_refusals_body_cap_and_defensive_headers() {
    let mut body = br#"{"error":{"type":"usage_limit_reached"}}"#.to_vec();
    body.resize(64 * 1024, b' ');
    for value in ["-1", "1.5", "Wed, 21 Oct 2026 07:28:00 GMT"] {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_str(value).unwrap());
        assert_eq!(
            classify_chatgpt_refusal(429, &body, &headers),
            Some(Refusal::UsageLimitReached {
                plan_type: None,
                resets_at: None,
                retry_after: None
            })
        );
    }
    let mut headers = HeaderMap::new();
    headers.append("retry-after", HeaderValue::from_static("1"));
    headers.append("retry-after", HeaderValue::from_static("2"));
    assert_eq!(
        classify_chatgpt_refusal(429, &body, &headers),
        Some(Refusal::UsageLimitReached {
            plan_type: None,
            resets_at: None,
            retry_after: None
        })
    );
    body.push(b' ');
    assert_eq!(classify_chatgpt_refusal(429, &body, &headers), None);
}
