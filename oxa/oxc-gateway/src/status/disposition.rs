//! Pure refusal policy for the #627 provider parity classes. Available with
//! `anthropic-client,pricing,ledger` (reuses the existing chrono dependency).
//! Callers supply already-classified refusals: this is not a second HTTP/body
//! classifier, scheduler, retry loop, or clock. Unknown/provider-mismatched input
//! fails closed. No provider text or credentials are retained.
use chrono::NaiveDateTime;
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAi,
    OpenRouter,
    ChatGpt,
}

/// Semantic classes from the provider classifiers. Organization/project spend
/// caps share SpendLimit; usage caps share UsageLimit. OpenRouter limit sources
/// remain distinct, especially its transient in-flight budget versus credit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    CreditExhausted,
    SpendLimit,
    UsageLimit,
    KeyLimit,
    GuardrailBudget,
    WorkspaceBudget,
    InFlightBudget,
    RateLimited,
    Overloaded,
    Unavailable,
    UsageLimitReached { resets_at: Option<SystemTime> },
    UsageNotIncluded,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    CreditExhausted,
    SpendLimit,
    UsageLimit,
    KeyLimit,
    GuardrailBudget,
    WorkspaceBudget,
    UsageNotIncluded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Stop(StopReason),
    WaitUntil(SystemTime),
    /// Retryable, but no deadline is invented. The caller applies its bounded
    /// retry policy and any parsed Retry-After; this function executes nothing.
    RetryLater,
    Fail,
}

/// One decision per known provider/class pair. Monetary/account caps stop even
/// if an upstream supplies Retry-After. Only the subscription's explicit reset
/// yields WaitUntil; a missing reset remains retryable, not a guessed timestamp.
pub fn decide(provider: Provider, refusal: Refusal) -> Disposition {
    use Disposition::{Fail, RetryLater, Stop, WaitUntil};
    use Provider::{Anthropic, ChatGpt, OpenAi, OpenRouter};
    use Refusal::*;
    match (provider, refusal) {
        (Anthropic | OpenAi | OpenRouter, CreditExhausted) => Stop(StopReason::CreditExhausted),
        (Anthropic | OpenAi, SpendLimit) => Stop(StopReason::SpendLimit),
        (Anthropic | OpenAi, UsageLimit) => Stop(StopReason::UsageLimit),
        (OpenRouter, KeyLimit) => Stop(StopReason::KeyLimit),
        (OpenRouter, GuardrailBudget) => Stop(StopReason::GuardrailBudget),
        (OpenRouter, WorkspaceBudget) => Stop(StopReason::WorkspaceBudget),
        (Anthropic | OpenAi | OpenRouter, RateLimited | Overloaded)
        | (OpenRouter, InFlightBudget | Unavailable) => RetryLater,
        (
            ChatGpt,
            UsageLimitReached {
                resets_at: Some(time),
            },
        ) => WaitUntil(time),
        (ChatGpt, UsageLimitReached { resets_at: None }) => RetryLater,
        (ChatGpt, UsageNotIncluded) => Stop(StopReason::UsageNotIncluded),
        _ => Fail,
    }
}

/// Parse delta-seconds or an IMF-fixdate HTTP date against an injected clock,
/// capped by the caller's maximum delay. Past dates yield zero. Invalid,
/// overflowing, duplicate/comma-joined and control-bearing values yield None.
/// This accepts the standard HTTP sender date format, not obsolete date forms.
/// Only optional HTTP whitespace (SP/HTAB) is stripped; raw values never escape.
pub fn parse_retry_after(value: &str, now: SystemTime, cap: Duration) -> Option<Duration> {
    let value = value.trim_matches([' ', '\t']);
    if value.is_empty() || value.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }
    let delay = if value.bytes().all(|byte| byte.is_ascii_digit()) {
        Duration::from_secs(value.parse().ok()?)
    } else {
        // Exact wire shape, English weekday/month and GMT. chrono also checks
        // calendar validity and that the weekday matches the date.
        if value.len() != 29 || !value.ends_with(" GMT") {
            return None;
        }
        let date = NaiveDateTime::parse_from_str(value, "%a, %d %b %Y %H:%M:%S GMT").ok()?;
        // Round-trip rejects lenient parser inputs (widths, case, leap seconds).
        if date.format("%a, %d %b %Y %H:%M:%S GMT").to_string() != value
            || date.and_utc().timestamp_subsec_nanos() != 0
        {
            return None;
        }
        let timestamp = date.and_utc().timestamp();
        let date = if timestamp >= 0 {
            SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(timestamp as u64))?
        } else {
            SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(timestamp.unsigned_abs()))?
        };
        date.duration_since(now).unwrap_or(Duration::ZERO)
    };
    Some(delay.min(cap))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(784111777)
    }

    #[test]
    fn parity_table_has_one_decision_per_refusal_class() {
        use Disposition::{Fail, RetryLater, Stop, WaitUntil};
        use Provider::{Anthropic, ChatGpt, OpenAi, OpenRouter};
        use Refusal::*;
        let until = now() + Duration::from_secs(60);
        // Labels are the wire classes from #627, not inputs to another classifier.
        let cases = [
            (
                Anthropic,
                "400 credit balance is too low",
                CreditExhausted,
                Stop(StopReason::CreditExhausted),
            ),
            (
                Anthropic,
                "enforced_spend_limit_reached",
                SpendLimit,
                Stop(StopReason::SpendLimit),
            ),
            (
                Anthropic,
                "400 specified API usage limits",
                UsageLimit,
                Stop(StopReason::UsageLimit),
            ),
            (Anthropic, "429 rate_limit_error", RateLimited, RetryLater),
            (Anthropic, "529 overloaded_error", Overloaded, RetryLater),
            (
                OpenAi,
                "credit_balance_exhausted",
                CreditExhausted,
                Stop(StopReason::CreditExhausted),
            ),
            (
                OpenAi,
                "organization_spend_limit_exceeded",
                SpendLimit,
                Stop(StopReason::SpendLimit),
            ),
            (
                OpenAi,
                "project_spend_limit_exceeded",
                SpendLimit,
                Stop(StopReason::SpendLimit),
            ),
            (
                OpenAi,
                "organization_usage_limit_exceeded",
                UsageLimit,
                Stop(StopReason::UsageLimit),
            ),
            (OpenAi, "slow_down", RateLimited, RetryLater),
            (OpenAi, "503 server_is_overloaded", Overloaded, RetryLater),
            (
                OpenRouter,
                "openrouter_credits",
                CreditExhausted,
                Stop(StopReason::CreditExhausted),
            ),
            (
                OpenRouter,
                "openrouter_in_flight_budget",
                InFlightBudget,
                RetryLater,
            ),
            (
                OpenRouter,
                "openrouter_key_limit",
                KeyLimit,
                Stop(StopReason::KeyLimit),
            ),
            (
                OpenRouter,
                "openrouter_guardrail_budget",
                GuardrailBudget,
                Stop(StopReason::GuardrailBudget),
            ),
            (
                OpenRouter,
                "openrouter_workspace_budget",
                WorkspaceBudget,
                Stop(StopReason::WorkspaceBudget),
            ),
            (OpenRouter, "provider_overloaded", Overloaded, RetryLater),
            (OpenRouter, "provider_unavailable", Unavailable, RetryLater),
            (OpenRouter, "429 rate limit", RateLimited, RetryLater),
            (
                ChatGpt,
                "usage_limit_reached",
                UsageLimitReached {
                    resets_at: Some(until),
                },
                WaitUntil(until),
            ),
            (
                ChatGpt,
                "usage_limit_reached without reset",
                UsageLimitReached { resets_at: None },
                RetryLater,
            ),
            (
                ChatGpt,
                "usage_not_included",
                UsageNotIncluded,
                Stop(StopReason::UsageNotIncluded),
            ),
            (OpenAi, "unknown", Unknown, Fail),
        ];
        for (provider, label, refusal, expected) in cases {
            assert_eq!(decide(provider, refusal), expected, "{provider:?}: {label}");
        }
    }

    #[test]
    fn unknown_or_wrong_provider_classes_fail_closed() {
        for provider in [
            Provider::Anthropic,
            Provider::OpenAi,
            Provider::OpenRouter,
            Provider::ChatGpt,
        ] {
            assert_eq!(decide(provider, Refusal::Unknown), Disposition::Fail);
        }
        for (provider, refusal) in [
            (Provider::Anthropic, Refusal::InFlightBudget),
            (Provider::OpenAi, Refusal::KeyLimit),
            (Provider::OpenRouter, Refusal::UsageNotIncluded),
            (Provider::ChatGpt, Refusal::CreditExhausted),
            (
                Provider::OpenAi,
                Refusal::UsageLimitReached {
                    resets_at: Some(now()),
                },
            ),
        ] {
            assert_eq!(decide(provider, refusal), Disposition::Fail);
        }
        // No clock read or invented reset: even an elapsed reset stays the caller's deadline.
        assert_eq!(
            decide(
                Provider::ChatGpt,
                Refusal::UsageLimitReached {
                    resets_at: Some(SystemTime::UNIX_EPOCH)
                }
            ),
            Disposition::WaitUntil(SystemTime::UNIX_EPOCH)
        );
    }

    #[test]
    fn retry_after_seconds_and_http_dates_are_bounded() {
        let cap = Duration::from_secs(120);
        for (value, seconds) in [
            ("0", 0),
            ("60", 60),
            (" 60\t", 60),
            ("001", 1),
            ("999999", 120),
            ("18446744073709551615", 120),
            ("Sun, 06 Nov 1994 08:49:37 GMT", 0),
            ("Sun, 06 Nov 1994 08:50:37 GMT", 60),
            ("Sun, 06 Nov 1994 09:49:37 GMT", 120),
            ("Sun, 06 Nov 1994 07:49:37 GMT", 0),
        ] {
            assert_eq!(
                parse_retry_after(value, now(), cap),
                Some(Duration::from_secs(seconds)),
                "{value}"
            );
        }
        assert_eq!(
            parse_retry_after("60", now(), Duration::ZERO),
            Some(Duration::ZERO)
        );
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:50:37 GMT", now(), Duration::ZERO),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn retry_after_rejects_invalid_or_ambiguous_values() {
        for value in [
            "",
            "-1",
            "+1",
            "1.5",
            "1e2",
            "1, 2",
            "18446744073709551616",
            "Sun, 31 Feb 1994 08:50:37 GMT",
            "Mon, 06 Nov 1994 08:50:37 GMT",
            "Sun, 06 Nov 1994 08:50:37 UTC",
            "Sun, 06 Nov 1994 08:50:37 GMT trailing",
            "Sun, 06 Nov 1994 08:50:37 GMT\r\n",
            "synthetic-invalid-header",
            "é",
        ] {
            assert_eq!(
                parse_retry_after(value, now(), Duration::from_secs(120)),
                None,
                "{value}"
            );
        }
    }
}
