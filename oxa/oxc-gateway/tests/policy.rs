#![cfg(feature = "policy")]
use oxc_gateway::policy::{before_credentials, daily_budget, Refusal};
use serde_json::json;

fn refusal(error: Refusal, status: u16, code: &str, kind: &str, param: &str) {
    assert_eq!(error.status, status);
    assert_eq!(error.code, code);
    assert_eq!(error.param, param);
    let body = error.body();
    let detail = &body["error"];
    assert_eq!(detail["code"], code);
    assert_eq!(detail["param"], param);
    assert_eq!(detail["type"], kind);
    assert!(detail["message"]
        .as_str()
        .is_some_and(|message| !message.trim().is_empty()));
}

#[test]
fn offline_and_shared_requests_are_refused_before_admission() {
    for (shared, offline, code) in [(true, false, "cloud_not_shared"), (false, true, "offline")] {
        refusal(
            before_credentials(&json!({}), shared, offline, true, true).unwrap_err(),
            403,
            code,
            "permission_error",
            "model",
        );
    }
}

#[test]
fn requests_cannot_multiply_the_reserved_completion() {
    for n in [json!(0), json!(2), json!(-1), json!(true), json!(1.0), json!("1")] {
        refusal(
            before_credentials(&json!({"n":n}), false, false, true, true).unwrap_err(),
            400,
            "invalid_value",
            "cloud_error",
            "n",
        );
    }
    for payload in [json!({}), json!({"n":1}), json!({"n":null})] {
        assert!(before_credentials(&payload, false, false, true, true).is_ok());
    }
}

#[test]
fn unknown_prices_and_unusable_budgets_have_distinct_client_errors() {
    refusal(
        before_credentials(&json!({}), false, false, false, true).unwrap_err(),
        400,
        "cloud_pricing_unavailable",
        "cloud_error",
        "pricing",
    );
    // An unset cap is no budget check; only a configured, unusable cap refuses.
    assert!(daily_budget(&json!(null), true, Some(0.0), 0.0, 0.0).is_ok());
    assert!(daily_budget(&json!(null), true, Some(5.0), 5.0, 5.0).is_ok());
    for cap in [json!(false), json!(-1), json!(0), json!("1")] {
        refusal(
            daily_budget(&cap, true, Some(0.0), 0.0, 0.0).unwrap_err(),
            403,
            "budget_not_configured",
            "permission_error",
            "model",
        );
    }
}

#[test]
fn spending_and_outstanding_holds_both_reduce_available_budget() {
    for (spent, held, required) in [
        (0.5, 0.4, 0.2),
        (0.999, 0.0, 0.08008),
        (0.0, 1.0, 1e-13),
        (1.0, 0.0, 1e-13),
    ] {
        refusal(
            daily_budget(&json!(1), true, Some(spent), held, required).unwrap_err(),
            429,
            "budget_exceeded",
            "rate_limit_error",
            "model",
        );
    }
    assert!(daily_budget(&json!(1), true, Some(0.5), 0.0, 0.2).is_ok());
    assert!(daily_budget(&json!(1), true, Some(0.5), 0.4, 0.1).is_ok());
}

#[test]
fn unusable_ledger_or_invalid_accounting_has_a_clean_unavailable_error() {
    refusal(
        daily_budget(&json!(1), false, Some(0.0), 0.0, 0.1).unwrap_err(),
        503,
        "cloud_ledger_unavailable",
        "cloud_error",
        "model",
    );
    refusal(
        daily_budget(&json!(1), true, None, 0.0, 0.1).unwrap_err(),
        503,
        "cloud_ledger_unavailable",
        "cloud_error",
        "model",
    );
    for bad in [f64::NAN, f64::INFINITY, -1.0] {
        for (spent, held, required) in [(bad, 0.0, 0.1), (0.0, bad, 0.1), (0.0, 0.0, bad)] {
            refusal(
                daily_budget(&json!(1), true, Some(spent), held, required).unwrap_err(),
                503,
                "cloud_ledger_unavailable",
                "cloud_error",
                "model",
            );
        }
    }
}

#[test]
fn missing_credentials_and_ledger_errors_keep_their_public_classification() {
    refusal(
        Refusal::credentials(),
        503,
        "cloud_credentials_unavailable",
        "cloud_error",
        "model",
    );
    refusal(
        Refusal::ledger(),
        503,
        "cloud_ledger_unavailable",
        "cloud_error",
        "model",
    );
}

#[test]
fn null_cap_still_refuses_when_ledger_disabled() {
    // The absent cap skips only the cap check; the durable ledger stays a precondition.
    refusal(
        daily_budget(&json!(null), false, Some(0.0), 0.0, 0.0).unwrap_err(),
        503,
        "cloud_ledger_unavailable",
        "cloud_error",
        "model",
    );
}

#[test]
fn unpriced_model_admitted_without_cap() {
    assert!(before_credentials(&json!({}), false, false, false, false).is_ok());
    assert_eq!(
        before_credentials(&json!({}), false, false, false, true)
            .unwrap_err()
            .code,
        "cloud_pricing_unavailable"
    );
    // Sharing and offline stay refused whatever the pricing.
    assert_eq!(
        before_credentials(&json!({}), true, false, false, false)
            .unwrap_err()
            .code,
        "cloud_not_shared"
    );
}
