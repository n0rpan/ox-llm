#![cfg(feature = "policy")]
use oxc_gateway::policy::{before_credentials, daily_budget};
use serde_json::json;

#[test]
fn offline_and_shared_requests_are_refused_before_admission() {
    for (shared, offline, code) in [(true, false, "cloud_not_shared"), (false, true, "offline")] {
        let error = before_credentials(&json!({}), shared, offline, true).unwrap_err();
        assert_eq!(error.status, 403);
        assert_eq!(error.code, code);
        assert_eq!(error.body()["error"]["code"], code);
    }
}

#[test]
fn requests_cannot_multiply_the_reserved_completion() {
    for n in [json!(0), json!(2), json!(-1), json!(true), json!(1.0), json!("1")] {
        let error = before_credentials(&json!({"n":n}), false, false, true).unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(error.param, "n");
    }
    assert!(before_credentials(&json!({"n":1}), false, false, true).is_ok());
}

#[test]
fn unknown_prices_and_missing_budget_never_admit_a_call() {
    assert!(before_credentials(&json!({}), false, false, false).is_err());
    for cap in [json!(null), json!(false), json!(-1), json!(0), json!("1")] {
        assert!(daily_budget(&cap, true, Some(0.0), 0.0, 0.0).is_err());
    }
}

#[test]
fn spending_and_outstanding_holds_both_reduce_available_budget() {
    assert!(daily_budget(&json!(1), true, Some(0.5), 0.4, 0.2).is_err());
    assert!(daily_budget(&json!(1), true, Some(0.5), 0.0, 0.2).is_ok());
    assert!(daily_budget(&json!(1), true, Some(0.5), 0.4, 0.1).is_ok());
}

#[test]
fn unusable_ledger_or_invalid_accounting_fails_closed() {
    assert!(daily_budget(&json!(1), false, Some(0.0), 0.0, 0.1).is_err());
    assert!(daily_budget(&json!(1), true, None, 0.0, 0.1).is_err());
    for bad in [f64::NAN, f64::INFINITY, -1.0] {
        assert!(daily_budget(&json!(1), true, Some(bad), 0.0, 0.1).is_err());
        assert!(daily_budget(&json!(1), true, Some(0.0), bad, 0.1).is_err());
        assert!(daily_budget(&json!(1), true, Some(0.0), 0.0, bad).is_err());
    }
}
