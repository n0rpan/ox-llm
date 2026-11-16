//! Passive provider response metadata; no requests, caching, or retry policy.
//! Uses the existing `anthropic-client` (HTTP/JSON) and `ledger` (time) dependencies.
use chrono::{DateTime, FixedOffset};
use reqwest::header::HeaderMap;
use std::time::Duration;

pub mod chatgpt;
pub use chatgpt::classify_chatgpt_refusal;

// Keep the same refusal result as the native Anthropic discovery helper.
pub use crate::anthropic_client::models::Refusal;

/// A missing or malformed individual header is `None`, never an invented zero.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Limit<R> {
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub reset: Option<R>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenAiLimits {
    pub requests: Limit<Duration>,
    pub tokens: Limit<Duration>,
    pub retry_after: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnthropicLimits {
    pub requests: Limit<DateTime<FixedOffset>>,
    pub tokens: Limit<DateTime<FixedOffset>>,
    pub input_tokens: Limit<DateTime<FixedOffset>>,
    pub output_tokens: Limit<DateTime<FixedOffset>>,
    pub retry_after: Option<Duration>,
}

/// Parse the OpenAI API's relative reset durations. Components are unsigned
/// integers with units `h`, `m`, `s`, `ms`, in descending order, without repeats.
/// Parsing consumes the entire value and uses checked integer arithmetic.
pub fn openai_limits(headers: &HeaderMap) -> OpenAiLimits {
    let limit = |dimension| Limit {
        limit: header(headers, &format!("x-ratelimit-limit-{dimension}")).and_then(unsigned),
        remaining: header(headers, &format!("x-ratelimit-remaining-{dimension}")).and_then(unsigned),
        reset: header(headers, &format!("x-ratelimit-reset-{dimension}")).and_then(duration),
    };
    OpenAiLimits {
        requests: limit("requests"),
        tokens: limit("tokens"),
        retry_after: retry_after(headers),
    }
}

/// Parse Anthropic's absolute RFC 3339 reset timestamps, retaining their offset
/// and fractional seconds. Malformed fields do not discard other valid fields.
pub fn anthropic_limits(headers: &HeaderMap) -> AnthropicLimits {
    let limit = |dimension| Limit {
        limit: header(headers, &format!("anthropic-ratelimit-{dimension}-limit")).and_then(unsigned),
        remaining: header(headers, &format!("anthropic-ratelimit-{dimension}-remaining")).and_then(unsigned),
        reset: header(headers, &format!("anthropic-ratelimit-{dimension}-reset"))
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok()),
    };
    AnthropicLimits {
        requests: limit("requests"),
        tokens: limit("tokens"),
        input_tokens: limit("input-tokens"),
        output_tokens: limit("output-tokens"),
        retry_after: retry_after(headers),
    }
}

// Ambiguous duplicates and non-ASCII header values are malformed, not first-wins.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok()
}

fn unsigned(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    header(headers, "retry-after")
        .and_then(unsigned)
        .map(Duration::from_secs)
}

fn duration(mut value: &str) -> Option<Duration> {
    if value.is_empty() {
        return None;
    }
    let mut total = Duration::ZERO;
    let mut previous_rank = 4;
    while !value.is_empty() {
        let digits = value.bytes().take_while(u8::is_ascii_digit).count();
        let number = unsigned(&value[..digits])?;
        value = &value[digits..];
        let (unit, rank, component) = if value.starts_with("ms") {
            ("ms", 0, Duration::from_millis(number))
        } else if value.starts_with('s') {
            ("s", 1, Duration::from_secs(number))
        } else if value.starts_with('m') {
            ("m", 2, Duration::from_secs(number).checked_mul(60)?)
        } else if value.starts_with('h') {
            ("h", 3, Duration::from_secs(number).checked_mul(3600)?)
        } else {
            return None;
        };
        if rank >= previous_rank {
            return None;
        }
        previous_rank = rank;
        total = total.checked_add(component)?;
        value = &value[unit.len()..];
    }
    Some(total)
}

/// Classify by `error.code`, never `error.type`. Credit and organization/project
/// spend or usage caps share the non-retryable `OutOfCredit` result with Anthropic.
/// Unknown 429 bodies still indicate a rate limit; other unknown errors are `None`.
pub fn classify_openai_refusal(status: u16, body: &[u8], headers: &HeaderMap) -> Option<Refusal> {
    let body = serde_json::from_slice::<serde_json::Value>(body).ok();
    let code = body
        .as_ref()
        .and_then(|body| body.get("error"))
        .and_then(|error| error.get("code"));
    classify_openai_code(status, code.and_then(serde_json::Value::as_str), headers)
}

fn classify_openai_code(status: u16, code: Option<&str>, headers: &HeaderMap) -> Option<Refusal> {
    match (status, code) {
        (
            429,
            Some(
                "credit_balance_exhausted"
                | "organization_spend_limit_exceeded"
                | "project_spend_limit_exceeded"
                | "organization_usage_limit_exceeded"
                | "insufficient_quota",
            ),
        ) => Some(Refusal::OutOfCredit),
        (429, _) => Some(Refusal::RateLimited {
            retry_after: retry_after(headers),
        }),
        (503, Some("server_is_overloaded")) => Some(Refusal::Overloaded),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};
    use serde_json::json;

    fn headers(entries: &[(&str, &str)]) -> HeaderMap {
        entries
            .iter()
            .map(|(name, value)| {
                (
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_str(value).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn openai_all_headers_and_exact_durations() {
        let h = headers(&[
            ("X-RateLimit-Limit-Requests", "100"),
            ("x-ratelimit-remaining-requests", "0"),
            ("x-ratelimit-reset-requests", "6m0s"),
            ("x-ratelimit-limit-tokens", "2000"),
            ("x-ratelimit-remaining-tokens", "1999"),
            ("x-ratelimit-reset-tokens", "2ms"),
            ("retry-after", "3"),
        ]);
        let parsed = openai_limits(&h);
        assert_eq!(
            parsed.requests,
            Limit {
                limit: Some(100),
                remaining: Some(0),
                reset: Some(Duration::from_secs(360))
            }
        );
        assert_eq!(
            parsed.tokens,
            Limit {
                limit: Some(2000),
                remaining: Some(1999),
                reset: Some(Duration::from_millis(2))
            }
        );
        assert_eq!(parsed.retry_after, Some(Duration::from_secs(3)));
        for (text, expected) in [
            ("0s", Duration::ZERO),
            ("1h2m3s", Duration::from_secs(3723)),
            ("1s2ms", Duration::from_millis(1002)),
            ("1h2m3s4ms", Duration::from_millis(3723004)),
            ("18446744073709551615s", Duration::from_secs(u64::MAX)),
        ] {
            assert_eq!(
                openai_limits(&headers(&[("x-ratelimit-reset-tokens", text)]))
                    .tokens
                    .reset,
                Some(expected),
                "{text}"
            );
        }
    }

    #[test]
    fn absent_headers_and_invalid_fields_are_none_independently() {
        assert_eq!(openai_limits(&HeaderMap::new()), OpenAiLimits::default());
        assert_eq!(anthropic_limits(&HeaderMap::new()), AnthropicLimits::default());
        for invalid in ["", "-1", "+1", "1.0", " 1", "1 ", "1x", "18446744073709551616"] {
            let parsed = openai_limits(&headers(&[
                ("x-ratelimit-limit-requests", invalid),
                ("x-ratelimit-remaining-requests", invalid),
                ("x-ratelimit-limit-tokens", "2"),
                ("retry-after", invalid),
            ]));
            assert_eq!(parsed.requests.limit, None, "{invalid}");
            assert_eq!(parsed.requests.remaining, None, "{invalid}");
            assert_eq!(parsed.retry_after, None, "{invalid}");
            assert_eq!(parsed.tokens.limit, Some(2));
        }
        for invalid in [
            "",
            "1",
            "1msjunk",
            "1s!",
            "1.5s",
            "-1s",
            "+1s",
            "1 s",
            "1s ",
            "1d",
            "1s2m",
            "1s2s",
            "1m2h",
            "ms",
            "18446744073709551616s",
            "18446744073709551615h",
            "18446744073709551615s1000ms",
        ] {
            assert_eq!(
                openai_limits(&headers(&[("x-ratelimit-reset-tokens", invalid)]))
                    .tokens
                    .reset,
                None,
                "{invalid}"
            );
        }
        let mut h = HeaderMap::new();
        h.insert(
            "x-ratelimit-limit-tokens",
            HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        assert_eq!(openai_limits(&h).tokens.limit, None);
        h = headers(&[("x-ratelimit-limit-tokens", "1")]);
        h.append("x-ratelimit-limit-tokens", HeaderValue::from_static("2"));
        assert_eq!(openai_limits(&h).tokens.limit, None);
    }

    #[test]
    fn anthropic_all_dimensions_and_rfc3339() {
        let mut h = headers(&[("retry-after", "0")]);
        let reset = "2026-11-16T08:31:00.123456789+01:30";
        for dimension in ["requests", "tokens", "input-tokens", "output-tokens"] {
            for (field, value) in [("limit", "100"), ("remaining", "0"), ("reset", reset)] {
                h.insert(
                    HeaderName::from_bytes(format!("anthropic-ratelimit-{dimension}-{field}").as_bytes())
                        .unwrap(),
                    HeaderValue::from_str(value).unwrap(),
                );
            }
        }
        let parsed = anthropic_limits(&h);
        for limit in [
            parsed.requests,
            parsed.tokens,
            parsed.input_tokens,
            parsed.output_tokens,
        ] {
            assert_eq!(limit.limit, Some(100));
            assert_eq!(limit.remaining, Some(0));
            assert_eq!(limit.reset, Some(DateTime::parse_from_rfc3339(reset).unwrap()));
        }
        assert_eq!(parsed.retry_after, Some(Duration::ZERO));
        let utc = "2026-11-16T08:31:00Z";
        assert_eq!(
            anthropic_limits(&headers(&[("anthropic-ratelimit-tokens-reset", utc)]))
                .tokens
                .reset,
            Some(DateTime::parse_from_rfc3339(utc).unwrap())
        );
    }

    #[test]
    fn anthropic_malformed_headers() {
        for invalid in [
            "",
            "tomorrow",
            "2ms",
            "2026-02-30T00:00:00Z",
            "2026-11-16",
            "2026-11-16T08:31:00",
            "2026-11-16T08:31:00Zjunk",
        ] {
            assert_eq!(
                anthropic_limits(&headers(&[("anthropic-ratelimit-tokens-reset", invalid)]))
                    .tokens
                    .reset,
                None,
                "{invalid}"
            );
        }
        let parsed = anthropic_limits(&headers(&[
            ("anthropic-ratelimit-input-tokens-limit", "-1"),
            ("anthropic-ratelimit-output-tokens-remaining", "1.2"),
            ("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT"),
        ]));
        assert_eq!(parsed, AnthropicLimits::default());
    }

    #[test]
    fn openai_credit_and_spend_codes_are_not_retryable() {
        for code in [
            "credit_balance_exhausted",
            "organization_spend_limit_exceeded",
            "project_spend_limit_exceeded",
            "organization_usage_limit_exceeded",
            "insufficient_quota",
        ] {
            // A contradictory type must never override the code.
            let body =
                serde_json::to_vec(&json!({"error": {"code": code, "type": "rate_limit_error"}})).unwrap();
            let refusal = classify_openai_refusal(429, &body, &headers(&[("retry-after", "2")])).unwrap();
            assert_eq!(refusal, Refusal::OutOfCredit, "{code}");
            assert!(!refusal.is_retryable());
            assert_eq!(classify_openai_refusal(400, &body, &HeaderMap::new()), None);
        }
    }

    #[test]
    fn openai_rate_limits_ignore_error_type_and_tolerate_bad_bodies() {
        for body in [
            br#"{"error":{"code":"slow_down","type":"insufficient_quota"}}"#.as_slice(),
            br#"{"error":{"code":"unknown","type":"insufficient_quota"}}"#,
            br#"{"error":{"type":"insufficient_quota"}}"#,
            br#"{"error":{"code":42}}"#,
            b"null",
            b"[]",
            b"not json",
        ] {
            let refusal = classify_openai_refusal(429, body, &headers(&[("retry-after", "7")]));
            assert_eq!(
                refusal,
                Some(Refusal::RateLimited {
                    retry_after: Some(Duration::from_secs(7))
                })
            );
            assert!(refusal.unwrap().is_retryable());
            for h in [HeaderMap::new(), headers(&[("retry-after", "oops")])] {
                assert_eq!(
                    classify_openai_refusal(429, body, &h),
                    Some(Refusal::RateLimited { retry_after: None })
                );
            }
        }
    }

    #[test]
    fn openai_overloaded_and_unrecognized_errors() {
        let body = br#"{"error":{"code":"server_is_overloaded","type":"ignored"}}"#;
        assert_eq!(
            classify_openai_refusal(503, body, &HeaderMap::new()),
            Some(Refusal::Overloaded)
        );
        assert!(Refusal::Overloaded.is_retryable());
        for (status, body) in [
            (400, body.as_slice()),
            (200, body.as_slice()),
            (503, br#"{"error":{"type":"server_is_overloaded"}}"#.as_slice()),
            (503, b"malformed".as_slice()),
            (401, b"{}".as_slice()),
        ] {
            assert_eq!(classify_openai_refusal(status, body, &HeaderMap::new()), None);
        }
    }
}
