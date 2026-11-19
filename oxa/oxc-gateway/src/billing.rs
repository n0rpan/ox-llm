//! Explicit billing classes; a subscription never substitutes for missing pricing.
use crate::{policy::Refusal, pricing::Price};

#[derive(Clone, Debug)]
pub enum Billing {
    Metered(Price),
    Subscription,
}

/// Owner-selected limits, shared by all subscription routes for a calendar day.
/// An explicitly configured pair is positive in both dimensions. Go uses it for warnings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubscriptionLimits {
    requests: u64,
    tokens: u64,
}
impl SubscriptionLimits {
    pub fn new(requests: u64, tokens: u64) -> Result<Self, Refusal> {
        if requests == 0 || tokens == 0 {
            return Err(Self::missing());
        }
        Ok(Self { requests, tokens })
    }
    pub fn requests(self) -> u64 {
        self.requests
    }
    pub fn tokens(self) -> u64 {
        self.tokens
    }
    pub(crate) fn missing() -> Refusal {
        Refusal {
            status: 403,
            code: "subscription_limits_not_configured",
            param: "model",
            message: "Subscription models need positive daily request and token limits.",
        }
    }
    /// The owner's limits warn at 80% and 100% of a projected day and never refuse.
    pub(crate) fn advisory_warning(self, total: Counts, go: bool) -> Option<&'static str> {
        let full = total.requests >= self.requests || total.tokens >= self.tokens;
        let near = u128::from(total.requests) * 5 >= u128::from(self.requests) * 4
            || u128::from(total.tokens) * 5 >= u128::from(self.tokens) * 4;
        match (go, full, near) {
            (true, true, _) => Some("OpenCode Go: projected usage reached 100% of oxa.cloud.subscription_limits; continuing."),
            (true, false, true) => Some("OpenCode Go: projected usage reached 80% of oxa.cloud.subscription_limits; continuing."),
            (false, true, _) => Some("ChatGPT plan: projected usage reached 100% of oxa.cloud.subscription_limits; continuing."),
            (false, false, true) => Some("ChatGPT plan: projected usage reached 80% of oxa.cloud.subscription_limits; continuing."),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub requests: u64,
    pub tokens: u64,
}
impl Counts {
    pub fn add(self, other: Self) -> Result<Self, Refusal> {
        Ok(Self {
            requests: self
                .requests
                .checked_add(other.requests)
                .ok_or_else(Refusal::ledger)?,
            tokens: self
                .tokens
                .checked_add(other.tokens)
                .ok_or_else(Refusal::ledger)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_limits_are_positive_and_only_ever_warn() {
        for (requests, tokens) in [(0, 1), (1, 0), (0, 0)] {
            let error = SubscriptionLimits::new(requests, tokens).unwrap_err();
            assert_eq!(
                (error.status, error.code),
                (403, "subscription_limits_not_configured")
            );
        }
        let limits = SubscriptionLimits::new(3, 100).unwrap();
        let used = Counts {
            requests: 1,
            tokens: 20,
        };
        let held = Counts {
            requests: 1,
            tokens: 40,
        };
        let total = used.add(held).unwrap();
        // 2 of 3 requests and 60 of 100 tokens is below the first warning.
        let warn = |extra: Counts, go| limits.advisory_warning(total.add(extra).unwrap(), go);
        assert_eq!(warn(Counts { requests: 0, tokens: 0 }, true), None);
        // The projected day reaches 80% of the tokens: advisory, naming the route.
        assert!(warn(Counts { requests: 0, tokens: 20 }, true).unwrap().contains("80%"));
        assert!(warn(Counts { requests: 1, tokens: 0 }, false).unwrap().starts_with("ChatGPT plan"));
        // Past 100% still only warns; there is no refusal path.
        assert!(warn(Counts { requests: 5, tokens: 500 }, false).unwrap().contains("100%"));
    }

    #[test]
    fn overflow_closes_accounting_instead_of_wrapping_to_a_fresh_allowance() {
        for (used, required) in [
            (
                Counts {
                    requests: u64::MAX,
                    tokens: 0,
                },
                Counts {
                    requests: 1,
                    tokens: 0,
                },
            ),
            (
                Counts {
                    requests: 0,
                    tokens: u64::MAX,
                },
                Counts {
                    requests: 0,
                    tokens: 1,
                },
            ),
        ] {
            let error = used.add(required).unwrap_err();
            assert_eq!((error.status, error.code), (503, "cloud_ledger_unavailable"));
        }
    }
}
