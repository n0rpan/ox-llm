//! Pure provider status conversions and a twelve-line plain-text card.
//! Available with `anthropic-client,pricing`; no refresh, cache, clock read or I/O.
use crate::{anthropic_client::models, openrouter};
pub use bigdecimal::BigDecimal as Decimal;
use std::time::SystemTime;

/// Provenance of the monetary figure, not of a refusal state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    ProviderApi,
    LedgerEstimate,
    NotReported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Ok,
    /// Set by the caller's warning policy; conversions invent no threshold.
    Low,
    OutOfCredit,
    /// `None` means rate limited with no reported (or representable) deadline.
    RateLimitedUntil(Option<SystemTime>),
    SignedOut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderStatus {
    pub remaining_usd: Option<Decimal>,
    pub source: Source,
    pub state: State,
    pub checked_at: SystemTime,
}

impl From<(openrouter::Balance, SystemTime)> for ProviderStatus {
    fn from((balance, checked_at): (openrouter::Balance, SystemTime)) -> Self {
        // A key cannot spend more than either its own limit or account credit.
        let remaining = balance.limit_remaining.map_or_else(
            || balance.account_remaining.clone(),
            |key| key.min(balance.account_remaining.clone()),
        );
        let state = if remaining <= 0 {
            State::OutOfCredit
        } else {
            State::Ok
        };
        Self {
            remaining_usd: Some(remaining),
            source: Source::ProviderApi,
            state,
            checked_at,
        }
    }
}

impl ProviderStatus {
    /// Unknown refusals do not imply success. The caller retains provider-specific
    /// details (including OpenRouter's limit source) separately. `retry_at` must
    /// come from a reported deadline; do not substitute the check time.
    pub fn from_openrouter_refusal(
        kind: openrouter::RefusalKind,
        checked_at: SystemTime,
        retry_at: Option<SystemTime>,
    ) -> Option<Self> {
        use openrouter::RefusalKind;
        let state = match kind {
            RefusalKind::CreditExhausted | RefusalKind::KeyLimit => State::OutOfCredit,
            RefusalKind::RateLimit => State::RateLimitedUntil(retry_at),
            RefusalKind::Other => return None,
        };
        Some(Self::refused(state, checked_at))
    }

    /// Overload is not an account state. A refusal reports no monetary balance,
    /// even when it proves that credit is exhausted.
    pub fn from_anthropic_refusal(refusal: models::Refusal, checked_at: SystemTime) -> Option<Self> {
        let state = match refusal {
            models::Refusal::OutOfCredit => State::OutOfCredit,
            models::Refusal::RateLimited { retry_after } => {
                State::RateLimitedUntil(retry_after.and_then(|delay| checked_at.checked_add(delay)))
            }
            models::Refusal::Overloaded => return None,
        };
        Some(Self::refused(state, checked_at))
    }

    fn refused(state: State, checked_at: SystemTime) -> Self {
        Self {
            remaining_usd: None,
            source: Source::NotReported,
            state,
            checked_at,
        }
    }

    /// Render exactly twelve newline-terminated lines, with no terminal escapes,
    /// provider text or I/O. Timestamps are UTC Unix seconds, independent of locale.
    pub fn render_card(&self) -> String {
        let state = match self.state {
            State::Ok => "ok",
            State::Low => "low",
            State::OutOfCredit => "out of credit",
            State::RateLimitedUntil(_) => "rate limited",
            State::SignedOut => "signed out",
        };
        let source = match self.source {
            Source::ProviderApi => "provider API",
            Source::LedgerEstimate => "ledger estimate",
            Source::NotReported => "not reported",
        };
        let remaining = self
            .remaining_usd
            .as_ref()
            .map_or_else(|| "not reported".to_owned(), ToString::to_string);
        let until = match self.state {
            State::RateLimitedUntil(Some(time)) => unix_seconds(time),
            _ => "not reported".to_owned(),
        };
        format!(
            "Provider status\n\
             --------------\n\
             State: {state}\n\
             Remaining USD: {remaining}\n\
             Source: {source}\n\
             Checked at (Unix seconds): {}\n\
             Rate limited until (Unix seconds): {until}\n\
             \n\
             Amounts are in USD.\n\
             Ledger estimates are not provider balances.\n\
             Not reported does not mean zero.\n\
             No refresh is performed by this card.\n",
            unix_seconds(self.checked_at)
        )
    }
}

fn unix_seconds(time: SystemTime) -> String {
    time.duration_since(SystemTime::UNIX_EPOCH).map_or_else(
        |_| "before Unix epoch".to_owned(),
        |value| value.as_secs().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn checked_at() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(100)
    }

    fn balance(account: &str, key: Option<&str>) -> openrouter::Balance {
        openrouter::Balance {
            limit: None,
            limit_remaining: key.map(|value| value.parse().unwrap()),
            usage: Decimal::from(0),
            usage_daily: Decimal::from(0),
            usage_weekly: Decimal::from(0),
            usage_monthly: Decimal::from(0),
            limit_reset: None,
            account_remaining: account.parse().unwrap(),
        }
    }

    #[test]
    fn balance_uses_effective_credit_without_rounding() {
        for (account, key, expected) in [
            ("10.1234567890123456789", None, "10.1234567890123456789"),
            ("10", Some("2.000000000000000001"), "2.000000000000000001"),
            ("1", Some("5"), "1"),
        ] {
            let status = ProviderStatus::from((balance(account, key), checked_at()));
            assert_eq!(status.remaining_usd, Some(expected.parse().unwrap()));
            assert_eq!(status.source, Source::ProviderApi);
            assert_eq!(status.state, State::Ok);
            assert_eq!(status.checked_at, checked_at());
        }
    }

    #[test]
    fn zero_and_negative_effective_balances_are_exhausted() {
        for (account, key) in [("0", None), ("-0.01", None), ("5", Some("0"))] {
            assert_eq!(
                ProviderStatus::from((balance(account, key), checked_at())).state,
                State::OutOfCredit
            );
        }
    }

    #[test]
    fn openrouter_refusals_do_not_invent_money_or_success() {
        use openrouter::RefusalKind::*;
        for kind in [CreditExhausted, KeyLimit, RateLimit, Other] {
            let status = ProviderStatus::from_openrouter_refusal(kind, checked_at(), None);
            if kind == Other {
                assert_eq!(status, None);
                continue;
            }
            let status = status.unwrap();
            assert_eq!(status.remaining_usd, None);
            assert_eq!(status.source, Source::NotReported);
            assert_eq!(status.checked_at, checked_at());
            assert_eq!(
                status.state,
                if kind == RateLimit {
                    State::RateLimitedUntil(None)
                } else {
                    State::OutOfCredit
                }
            );
        }
        let until = checked_at() + Duration::from_secs(10);
        assert_eq!(
            ProviderStatus::from_openrouter_refusal(RateLimit, checked_at(), Some(until))
                .unwrap()
                .state,
            State::RateLimitedUntil(Some(until))
        );
    }

    #[test]
    fn anthropic_refusals_preserve_unknown_deadlines_and_overload() {
        use crate::anthropic_client::models::Refusal::*;
        for (refusal, state) in [
            (OutOfCredit, State::OutOfCredit),
            (RateLimited { retry_after: None }, State::RateLimitedUntil(None)),
            (
                RateLimited {
                    retry_after: Some(Duration::from_secs(7)),
                },
                State::RateLimitedUntil(Some(checked_at() + Duration::from_secs(7))),
            ),
        ] {
            let status = ProviderStatus::from_anthropic_refusal(refusal, checked_at()).unwrap();
            assert_eq!(status.state, state);
            assert_eq!(status.remaining_usd, None);
            assert_eq!(status.source, Source::NotReported);
            assert_eq!(status.checked_at, checked_at());
        }
        assert_eq!(
            ProviderStatus::from_anthropic_refusal(Overloaded, checked_at()),
            None
        );
    }

    #[test]
    fn card_is_twelve_plain_text_lines_and_labels_estimates() {
        let status = ProviderStatus {
            remaining_usd: Some("1.25".parse().unwrap()),
            source: Source::LedgerEstimate,
            state: State::Low,
            checked_at: checked_at(),
        };
        assert_eq!(
            status.render_card(),
            concat!(
                "Provider status\n",
                "--------------\n",
                "State: low\n",
                "Remaining USD: 1.25\n",
                "Source: ledger estimate\n",
                "Checked at (Unix seconds): 100\n",
                "Rate limited until (Unix seconds): not reported\n",
                "\n",
                "Amounts are in USD.\n",
                "Ledger estimates are not provider balances.\n",
                "Not reported does not mean zero.\n",
                "No refresh is performed by this card.\n"
            )
        );
    }

    #[test]
    fn card_covers_every_state_and_unknown_values() {
        for (state, label, deadline) in [
            (State::Ok, "ok", "not reported"),
            (State::Low, "low", "not reported"),
            (State::OutOfCredit, "out of credit", "not reported"),
            (State::SignedOut, "signed out", "not reported"),
            (State::RateLimitedUntil(None), "rate limited", "not reported"),
            (State::RateLimitedUntil(Some(checked_at())), "rate limited", "100"),
        ] {
            let status = ProviderStatus {
                remaining_usd: None,
                source: Source::NotReported,
                state,
                checked_at: SystemTime::UNIX_EPOCH - Duration::from_secs(1),
            };
            let card = status.render_card();
            assert_eq!(card.lines().count(), 12);
            assert!(!card.contains('\x1b'));
            assert!(card.contains(&format!("State: {label}\n")));
            assert!(card.contains("Remaining USD: not reported\n"));
            assert!(card.contains("Source: not reported\n"));
            assert!(card.contains("Checked at (Unix seconds): before Unix epoch\n"));
            assert!(card.contains(&format!("Rate limited until (Unix seconds): {deadline}\n")));
        }
    }
}
