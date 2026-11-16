#![cfg(all(
    feature = "anthropic-client",
    feature = "pricing",
    feature = "upstream-openai"
))]

use oxc_gateway::{
    anthropic_client::models::Refusal,
    openai::chatgpt_limits::{Allowance, Window},
    status::{render_cards, CardRow, Decimal, Provider, ProviderStatus, Remaining, Source, State},
};
use std::time::{Duration, SystemTime};

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(100)
}

fn window(used_percent: f64, window_minutes: u64, reset: u64) -> Window {
    Window {
        used_percent,
        window_minutes,
        resets_at: now() + Duration::from_secs(reset),
    }
}

#[test]
fn status_openai_refusals_and_signed_out() {
    for (refusal, expected) in [
        (Refusal::OutOfCredit, Some(State::OutOfCredit)),
        (
            Refusal::RateLimited { retry_after: None },
            Some(State::RateLimitedUntil(None)),
        ),
        (
            Refusal::RateLimited {
                retry_after: Some(Duration::from_secs(9)),
            },
            Some(State::RateLimitedUntil(Some(now() + Duration::from_secs(9)))),
        ),
        (Refusal::Overloaded, None),
    ] {
        let status = ProviderStatus::from_openai_refusal(refusal, now());
        assert_eq!(status.as_ref().map(|s| s.state), expected);
        if let Some(status) = status {
            assert_eq!(status.remaining, None);
            assert_eq!(status.source, Source::NotReported);
            assert_eq!(status.checked_at, now());
        }
    }
    let signed_out = ProviderStatus::signed_out(now());
    assert_eq!(signed_out.state, State::SignedOut);
    assert_eq!(signed_out.remaining, None);
    assert_eq!(signed_out.source, Source::NotReported);
}

#[test]
fn status_chatgpt_allowance_is_a_window_not_money() {
    for (primary, secondary, expected) in [
        (None, None, None),
        (Some(window(25.5, 300, 60)), None, Some(("74.5", 300, 60))),
        (None, Some(window(80.0, 10080, 90)), Some(("20", 10080, 90))),
        (
            Some(window(25.0, 300, 60)),
            Some(window(80.0, 10080, 90)),
            Some(("20", 10080, 90)),
        ),
        (Some(window(100.0, 300, 60)), None, Some(("0", 300, 60))),
        (Some(window(f64::NAN, 300, 60)), None, None),
        (Some(window(-1.0, 300, 60)), None, None),
        (Some(window(101.0, 300, 60)), None, None),
        (Some(window(20.0, 0, 60)), None, None),
        (Some(window(20.0, 300, 0)), None, None),
    ] {
        let status = ProviderStatus::from_chatgpt(&Allowance { primary, secondary }, now());
        let remaining = expected.map(|(percent, window_minutes, reset)| Remaining::WindowPercent {
            percent: percent.parse().unwrap(),
            window_minutes,
            resets_at: now() + Duration::from_secs(reset),
        });
        assert_eq!(status.remaining, remaining);
        assert_eq!(
            status.source,
            if expected.is_some() {
                Source::ProviderApi
            } else {
                Source::NotReported
            }
        );
        assert_eq!(
            status.state,
            if expected.is_some_and(|v| v.0 == "0") {
                State::RateLimitedUntil(Some(now() + Duration::from_secs(60)))
            } else {
                State::Ok
            }
        );
    }
}

#[test]
fn status_four_provider_card_exact_output() {
    let usd: Decimal = "12.34".parse().unwrap();
    let spend: Decimal = "0.25".parse().unwrap();
    let openrouter = ProviderStatus {
        remaining: Some(Remaining::Usd(usd)),
        source: Source::ProviderApi,
        state: State::Ok,
        checked_at: now(),
    };
    let anthropic = ProviderStatus::from_anthropic_refusal(Refusal::OutOfCredit, now()).unwrap();
    let openai = ProviderStatus::signed_out(now());
    let chatgpt = ProviderStatus::from_chatgpt(
        &Allowance {
            primary: Some(window(25.5, 300, 60)),
            secondary: None,
        },
        now(),
    );
    let rows = [
        CardRow {
            provider: Provider::OpenRouter,
            status: &openrouter,
            today_spend_usd: Some(&spend),
        },
        CardRow {
            provider: Provider::Anthropic,
            status: &anthropic,
            today_spend_usd: None,
        },
        CardRow {
            provider: Provider::OpenAi,
            status: &openai,
            today_spend_usd: Some(&spend),
        },
        CardRow {
            provider: Provider::ChatGpt,
            status: &chatgpt,
            today_spend_usd: None,
        },
    ];
    assert_eq!(render_cards(&rows, now() + Duration::from_secs(5)), concat!(
        "OpenRouter | remaining: $12.34 | today: $0.25 | checked: 5s ago\n",
        "Anthropic | out of credit | today: unknown | checked: 5s ago\n",
        "OpenAI | signed out | today: $0.25 | checked: 5s ago\n",
        "ChatGPT | remaining: 74.5% of 300-minute window (resets at Unix 160) | today: unknown | checked: 5s ago\n",
    ));
}

#[test]
fn status_cards_states_unknown_age_and_bound() {
    for (state, expected) in [
        (State::Ok, "remaining: not reported by the provider"),
        (State::Low, "low; remaining: not reported by the provider"),
        (State::OutOfCredit, "out of credit"),
        (State::SignedOut, "signed out"),
        (
            State::RateLimitedUntil(None),
            "rate limited; reset: not reported by the provider",
        ),
        (
            State::RateLimitedUntil(Some(now())),
            "rate limited until Unix 100",
        ),
    ] {
        let status = ProviderStatus {
            remaining: None,
            source: Source::NotReported,
            state,
            checked_at: now(),
        };
        let row = CardRow {
            provider: Provider::OpenAi,
            status: &status,
            today_spend_usd: None,
        };
        assert_eq!(
            render_cards(&[row], SystemTime::UNIX_EPOCH),
            format!("OpenAI | {expected} | today: unknown | checked: unknown age\n")
        );
    }
    let status = ProviderStatus::signed_out(now());
    let rows: Vec<_> = (0..20)
        .map(|_| CardRow {
            provider: Provider::OpenAi,
            status: &status,
            today_spend_usd: None,
        })
        .collect();
    assert_eq!(render_cards(&rows, now()).lines().count(), 12);
    assert_eq!(render_cards(&[], now()), "");
}

#[test]
fn status_cards_label_estimates_without_rounding_or_fabricating_spend() {
    let status = ProviderStatus {
        remaining: Some(Remaining::Usd("0.000000000000000001".parse().unwrap())),
        source: Source::LedgerEstimate,
        state: State::Low,
        checked_at: now(),
    };
    let spend = "1.234567890123456789".parse().unwrap();
    assert_eq!(
        render_cards(&[CardRow {
            provider: Provider::Anthropic,
            status: &status,
            today_spend_usd: Some(&spend),
        }], now()),
        "Anthropic | low; remaining: $1E-18 (ledger estimate) | today: $1.234567890123456789 | checked: 0s ago\n"
    );
}

#[test]
fn status_chatgpt_decimal_subtraction_and_invalid_window_fallback() {
    for invalid in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN, -1.0, 101.0] {
        let status = ProviderStatus::from_chatgpt(
            &Allowance {
                primary: Some(window(invalid, 300, 60)),
                secondary: Some(window(0.1, 10080, 90)),
            },
            now(),
        );
        assert_eq!(
            status.remaining,
            Some(Remaining::WindowPercent {
                percent: "99.9".parse().unwrap(),
                window_minutes: 10080,
                resets_at: now() + Duration::from_secs(90),
            })
        );
    }
    let status = ProviderStatus::from_chatgpt(
        &Allowance {
            primary: Some(window(50.0, 300, 60)),
            secondary: Some(window(50.0, 10080, 90)),
        },
        now(),
    );
    assert_eq!(
        status.remaining,
        Some(Remaining::WindowPercent {
            percent: Decimal::from(50),
            window_minutes: 10080,
            resets_at: now() + Duration::from_secs(90),
        })
    );
}
