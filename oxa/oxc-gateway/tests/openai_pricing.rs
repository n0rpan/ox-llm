#![cfg(all(feature = "upstream-openai", feature = "accounting"))]

//! Responses -> canonical usage -> accounting -> pricing composition only.
//! No transport, credentials, admission, billing policy or production changes.
use oxc_gateway::{
    accounting::Usage,
    openai::stream::Stream,
    pricing::{Cache, Price},
};
use serde_json::{json, Value};

fn translated(raw: Value) -> (Usage, bool) {
    let mut stream = Stream::new("synthetic-model", 7, true);
    let mut usage = Usage::default();
    // Provisional counts must not become trusted final usage if the terminal
    // response omits it. Only the real translator's canonical chunks are fed in.
    for event in [
        json!({"type":"response.created","response":{"status":"in_progress","usage":{"input_tokens":999,"output_tokens":999}}}),
        json!({"type":"response.completed","response":{"status":"completed","output":[],"usage":raw}}),
    ] {
        for chunk in stream.event(&event) {
            usage.observe(&chunk);
        }
    }
    let completed = stream.finish();
    assert!(!completed.failed);
    (usage, completed.usage.is_some())
}
fn price(write_rate: u64) -> Price {
    Price::from_model(&json!({"pricing":{
        "input_per_million":2,"output_per_million":7,
        "cache_read_per_million":0.25,"cache_write_per_million":write_rate
    }}))
    .unwrap()
}
fn cost(usage: &Usage, price: &Price) -> Option<f64> {
    // Unknown totals cannot be converted to zero before calling Price::cost.
    let (prompt, output) = usage.prompt.zip(usage.output)?;
    price.cost(
        "openai",
        "synthetic-model",
        prompt,
        output,
        Cache::Tokens {
            read: usage.cache_read.unwrap_or(0),
            write: usage.cache_write.unwrap_or(0),
        },
    )
}

#[test]
fn responses_cached_input_and_reasoning_are_each_charged_once() {
    let (usage, trusted) = translated(json!({
        "input_tokens":1000,"output_tokens":50,
        "input_tokens_details":{"cached_tokens":800},
        "output_tokens_details":{"reasoning_tokens":30}
    }));
    assert!(trusted);
    assert_eq!(usage.prompt, Some(1000));
    assert_eq!(usage.output, Some(50));
    assert_eq!(usage.cache_read, Some(800));
    // Responses has no cache writes. Canonical frames omit that detail, and
    // existing accounting uses zero writes; do not invent writes from reads.
    assert_eq!(usage.cache_write, None);
    assert!(!usage.invalid_cache);
    // 200 uncached * 2 + 800 cached * .25 + 50 total output * 7.
    // Reasoning is already included in 50, not another 30 output tokens.
    assert_eq!(cost(&usage, &price(3)), Some(0.00095));
    assert_eq!(cost(&usage, &price(999_999)), Some(0.00095));
}

#[test]
fn absent_optional_details_and_explicit_zero_totals_are_distinct_from_unknown_totals() {
    for (raw, expected) in [
        (json!({"input_tokens":10,"output_tokens":5}), 0.000055),
        (json!({"input_tokens":0,"output_tokens":0}), 0.0),
        (
            json!({"input_tokens":0,"output_tokens":0,"input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}),
            0.0,
        ),
    ] {
        let (usage, trusted) = translated(raw);
        assert!(trusted);
        assert_eq!(usage.cache_read, Some(0));
        assert_eq!(usage.cache_write, None);
        assert_eq!(cost(&usage, &price(999_999)), Some(expected));
    }
}

#[test]
fn missing_or_invalid_final_usage_stays_unknown_and_cannot_be_a_free_charge() {
    for raw in [
        Value::Null,
        json!({}),
        json!({"input_tokens":0}),
        json!({"output_tokens":0}),
        json!({"input_tokens":null,"output_tokens":0}),
        json!({"input_tokens":0,"output_tokens":null}),
        json!({"input_tokens":0,"output_tokens":"0"}),
        json!({"input_tokens":-1,"output_tokens":0}),
        json!({"input_tokens":1,"output_tokens":2,"input_tokens_details":{"cached_tokens":2}}),
        json!({"input_tokens":1,"output_tokens":2,"output_tokens_details":{"reasoning_tokens":3}}),
    ] {
        let (usage, trusted) = translated(raw);
        assert!(!trusted);
        assert_eq!(usage.prompt, None);
        assert_eq!(usage.output, None);
        assert_eq!(usage.cache_read, None);
        assert_eq!(usage.cache_write, None);
        assert_eq!(cost(&usage, &price(3)), None);
    }
}
