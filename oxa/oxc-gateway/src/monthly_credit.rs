//! Owner-configured credit periods. The caller supplies the policy calendar
//! and uses the existing gateway ledger to account for the returned window.
use chrono::{DateTime, Datelike, Months, NaiveDate, NaiveTime, TimeZone, Utc};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Boundary {
    day: u32,
    time: NaiveTime,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonthlyCredit {
    amount_usd: f64,
    refill: Boundary,
    expiry: Option<Boundary>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CreditPeriod {
    pub amount_usd: f64,
    pub starts_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub next_refill_at: DateTime<Utc>,
}

impl CreditPeriod {
    pub fn active_at(&self, now: DateTime<Utc>) -> bool {
        self.starts_at <= now && now < self.expires_at
    }
}

impl MonthlyCredit {
    /// Decode the `monthly_credit` stanza from protected owner policy.
    /// Missing times mean midnight. Missing expiry means the next refill.
    pub fn from_owner_config(raw: &Value) -> Result<Self, &'static str> {
        let fields = raw.as_object().ok_or("monthly credit must be an object")?;
        if fields.keys().any(|key| {
            !matches!(
                key.as_str(),
                "amount_usd" | "refill_day" | "refill_time" | "expiry_day" | "expiry_time"
            )
        }) {
            return Err("unknown monthly credit field");
        }
        let amount_usd = raw["amount_usd"]
            .as_f64()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .ok_or("invalid monthly credit amount")?;
        let refill = Boundary::parse(&raw["refill_day"], raw.get("refill_time"))?;
        let expiry = match (raw.get("expiry_day"), raw.get("expiry_time")) {
            (Some(day), time) => Some(Boundary::parse(day, time)?),
            (None, None) => None,
            (None, Some(_)) => return Err("monthly credit expiry time requires a day"),
        };
        Ok(Self {
            amount_usd,
            refill,
            expiry,
        })
    }

    /// Days beyond the month's length use its last day. Use the later instant
    /// for a repeated refill time and the earlier instant for a repeated expiry.
    /// A skipped local boundary yields an unavailable period until it is resolved.
    pub fn period_at<Tz: TimeZone>(&self, zone: &Tz, now: DateTime<Utc>) -> Option<CreditPeriod> {
        let this_month = now.with_timezone(zone).date_naive().with_day(1)?;
        let this_refill = self.refill.in_month(zone, this_month, true)?;
        let (month, starts_at) = if this_refill <= now {
            (this_month, this_refill)
        } else {
            let previous = this_month.checked_sub_months(Months::new(1))?;
            (previous, self.refill.in_month(zone, previous, true)?)
        };
        let next_month = month.checked_add_months(Months::new(1))?;
        let next_refill_at = self.refill.in_month(zone, next_month, true)?;
        let expires_at = match self.expiry {
            None => next_refill_at,
            Some(expiry) => {
                let initial = expiry.in_month(zone, month, false)?;
                let after_start = if initial > starts_at {
                    initial
                } else {
                    expiry.in_month(zone, next_month, false)?
                };
                after_start.min(next_refill_at)
            }
        };
        Some(CreditPeriod {
            amount_usd: self.amount_usd,
            starts_at,
            expires_at,
            next_refill_at,
        })
    }
}

impl Boundary {
    fn parse(day: &Value, time: Option<&Value>) -> Result<Self, &'static str> {
        let day = day
            .as_u64()
            .filter(|day| (1..=31).contains(day))
            .ok_or("invalid monthly credit boundary day")? as u32;
        let time = match time {
            None => NaiveTime::default(),
            Some(time) => time
                .as_str()
                .filter(|time| time.len() == 5 && time.as_bytes()[2] == b':')
                .and_then(|time| NaiveTime::parse_from_str(time, "%H:%M").ok())
                .ok_or("monthly credit boundary time must be HH:MM")?,
        };
        Ok(Self { day, time })
    }

    fn in_month<Tz: TimeZone>(self, zone: &Tz, month: NaiveDate, refill: bool) -> Option<DateTime<Utc>> {
        let last_day = month.checked_add_months(Months::new(1))?.pred_opt()?.day();
        let instant = month.with_day(self.day.min(last_day))?.and_time(self.time);
        let mapped = zone.from_local_datetime(&instant);
        let boundary = if refill {
            mapped.latest()?
        } else {
            mapped.earliest()?
        };
        Some(boundary.to_utc())
    }
}
