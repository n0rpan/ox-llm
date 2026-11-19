#![cfg(feature = "ledger")]

use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use oxc_gateway::monthly_credit::MonthlyCredit;
use serde_json::json;

fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0).unwrap()
}

#[test]
fn default_expiry_follows_refills_across_short_months() {
    let credit = MonthlyCredit::from_owner_config(&json!({"amount_usd": 17.50, "refill_day": 31})).unwrap();
    let january = at(2028, 1, 31, 0, 0);
    let february = at(2028, 2, 29, 0, 0);
    let january_period = credit.period_at(&Utc, at(2028, 2, 28, 23, 59)).unwrap();
    assert_eq!(january_period.starts_at, january);
    assert_eq!(january_period.expires_at, february);
    assert_eq!(january_period.next_refill_at, february);
    assert!(!january_period.active_at(february));
    let february_period = credit.period_at(&Utc, february).unwrap();
    assert_eq!(february_period.starts_at, february);
    assert_eq!(february_period.expires_at, at(2028, 3, 31, 0, 0));
    assert!(february_period.active_at(february));
}

#[test]
fn owner_calendar_keeps_expiry_and_refill_distinct_across_new_year() {
    let zone = FixedOffset::east_opt(3600).unwrap();
    let raw = json!({
        "amount_usd": 41,
        "refill_day": 15, "refill_time": "09:30",
        "expiry_day": 5, "expiry_time": "18:10"
    });
    let credit = MonthlyCredit::from_owner_config(&raw).unwrap();
    let expiry = at(2027, 1, 5, 17, 10);
    let period = credit.period_at(&zone, expiry).unwrap();
    assert_eq!(period.amount_usd, 41.0);
    assert_eq!(period.starts_at, at(2026, 12, 15, 8, 30));
    assert_eq!(period.expires_at, expiry);
    assert_eq!(period.next_refill_at, at(2027, 1, 15, 8, 30));
    assert!(period.active_at(at(2027, 1, 5, 17, 9)));
    assert!(!period.active_at(expiry));

    let mut changed_policy = raw;
    changed_policy["amount_usd"] = json!(9);
    changed_policy["expiry_time"] = json!("20:00");
    let changed = MonthlyCredit::from_owner_config(&changed_policy)
        .unwrap()
        .period_at(&zone, expiry)
        .unwrap();
    assert_eq!(changed.amount_usd, 9.0);
    assert_eq!(changed.expires_at, at(2027, 1, 5, 19, 0));
    assert!(changed.active_at(expiry));
}

#[test]
fn rejects_invalid_amounts_and_malformed_boundaries() {
    let valid = json!({"amount_usd": 0, "refill_day": 1});
    assert!(MonthlyCredit::from_owner_config(&valid).is_ok());
    for (key, value) in [
        ("amount_usd", json!(-1)),
        ("amount_usd", json!("25")),
        ("refill_day", json!(0)),
        ("refill_day", json!(32)),
        ("refill_day", json!(1.5)),
        ("refill_time", json!("24:00")),
        ("refill_time", json!("2:00")),
        ("expiry_time", json!("10:00")),
        ("expiry_day", json!(null)),
        ("expire_day", json!(10)),
    ] {
        let mut raw = valid.clone();
        raw[key] = value;
        assert!(MonthlyCredit::from_owner_config(&raw).is_err(), "{key}");
    }
}
