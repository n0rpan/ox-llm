#![cfg(all(feature = "anthropic-client", feature = "pricing"))]
#[allow(dead_code)] // This parser fixture does not exercise Responses streaming.
#[path = "common/openai.rs"]
mod mock;
use oxc_gateway::status::chatgpt_usage::{parse, Credits, Usage, Window};
use serde_json::{json, Value};

#[tokio::test]
async fn chatgpt_usage_synthetic_reply() {
    let mock = mock::MockOpenAi::start(vec![mock::Reply::chatgpt_usage()]);
    let body: Value = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("http://{}/backend-api/wham/usage", mock.address()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        parse(&body),
        Usage {
            plan_type: Some("synthetic-plan".into()),
            primary: Some(Window {
                used_percent: Some(12.5),
                reset_at: Some(1_800_000_000)
            }),
            secondary: Some(Window {
                used_percent: Some(75.0),
                reset_at: Some(1_800_600_000)
            }),
            credits: Some(Credits {
                has_credits: Some(true),
                unlimited: Some(false),
                balance: Some("42.50".into())
            }),
        }
    );
    assert_eq!(mock.requests()[0].target, "/backend-api/wham/usage");
}

#[test]
fn chatgpt_usage_missing_and_wrong_containers() {
    for body in [
        Value::Null,
        json!(false),
        json!(0),
        json!("unknown"),
        json!([]),
        json!({}),
        json!({"unknown": {"future": true}}),
        json!({"plan_type": 7, "rate_limit": [], "credits": false}),
        json!({"rate_limit": {"primary_window": 3, "secondary_window": "unknown"}}),
    ] {
        assert_eq!(parse(&body), Usage::default(), "{body}");
    }
}

#[test]
fn chatgpt_usage_fields_are_independently_optional() {
    for wrong in [Value::Null, json!(true), json!([]), json!({}), json!("123")] {
        let usage = parse(&json!({
            "rate_limit": {"primary_window": {"used_percent": wrong, "reset_at": wrong}},
            "credits": {"has_credits": 1, "unlimited": "false", "balance": 42}
        }));
        assert_eq!(usage.primary, Some(Window::default()));
        assert_eq!(usage.credits, Some(Credits::default()));
    }
    let usage = parse(&json!({
        "plan_type": "future-plan",
        "rate_limit": {
            "primary_window": {"used_percent": 0, "unknown": true},
            "secondary_window": {"reset_at": 0}
        },
        "credits": {"unlimited": true, "balance": "0", "future": []},
        "spend_control": {"future": true}
    }));
    assert_eq!(usage.plan_type.as_deref(), Some("future-plan"));
    assert_eq!(
        usage.primary,
        Some(Window {
            used_percent: Some(0.0),
            reset_at: None
        })
    );
    assert_eq!(
        usage.secondary,
        Some(Window {
            used_percent: None,
            reset_at: Some(0)
        })
    );
    assert_eq!(
        usage.credits,
        Some(Credits {
            has_credits: None,
            unlimited: Some(true),
            balance: Some("0".into())
        })
    );
    for reset in [json!(-1), json!(1.5), json!("1800000000")] {
        assert_eq!(
            parse(&json!({"rate_limit": {"primary_window": {"reset_at": reset}}})).primary,
            Some(Window::default())
        );
    }
}
