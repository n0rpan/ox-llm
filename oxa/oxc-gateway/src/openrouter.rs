//! OpenRouter provider facts retained for status projection.
use bigdecimal::BigDecimal;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balance {
    pub limit: Option<BigDecimal>,
    pub limit_remaining: Option<BigDecimal>,
    pub usage: BigDecimal,
    pub usage_daily: BigDecimal,
    pub usage_weekly: BigDecimal,
    pub usage_monthly: BigDecimal,
    pub limit_reset: Option<String>,
    pub account_remaining: BigDecimal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalKind {
    CreditExhausted,
    KeyLimit,
    RateLimit,
    Other,
}
