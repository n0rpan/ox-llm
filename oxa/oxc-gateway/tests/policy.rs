#![cfg(feature = "policy")]
use oxc_gateway::policy::{before_credentials, daily_budget, Refusal};
use serde_json::json;

fn refusal(error: Refusal, status: u16, code: &str, param: &str) {
    assert_eq!(error.status, status);
    assert_eq!(error.code, code);
    assert_eq!(error.param, param);
    let body = error.body();
    let detail = &body["error"];
    assert_eq!(detail["code"], code);
    assert_eq!(detail["param"], param);
    assert_eq!(
        detail["type"],
        match status {
            403 => "permission_error",
            429 => "rate_limit_error",
            _ => "cloud_error",
        }
    );
    assert!(detail["message"]
        .as_str()
        .is_some_and(|message| !message.trim().is_empty()));
}

#[test]
fn offline_and_shared_requests_are_refused_before_admission() {
    for (shared, offline, code) in [(true, false, "cloud_not_shared"), (false, true, "offline")] {
        refusal(
            before_credentials(&json!({}), shared, offline, true).unwrap_err(),
            403,
            code,
            "model",
        );
    }
}

#[test]
fn requests_cannot_multiply_the_reserved_completion() {
    for n in [json!(0), json!(2), json!(-1), json!(true), json!(1.0), json!("1")] {
        refusal(
            before_credentials(&json!({"n":n}), false, false, true).unwrap_err(),
            400,
            "invalid_value",
            "n",
        );
    }
    for payload in [json!({}), json!({"n":1}), json!({"n":null})] {
        assert!(before_credentials(&payload, false, false, true).is_ok());
    }
}

#[test]
fn unknown_prices_and_missing_budget_have_distinct_client_errors() {
    refusal(
        before_credentials(&json!({}), false, false, false).unwrap_err(),
        400,
        "cloud_pricing_unavailable",
        "pricing",
    );
    for cap in [json!(null), json!(false), json!(-1), json!(0), json!("1")] {
        refusal(
            daily_budget(&cap, true, Some(0.0), 0.0, 0.0).unwrap_err(),
            403,
            "budget_not_configured",
            "model",
        );
    }
}

#[test]
fn spending_and_outstanding_holds_both_reduce_available_budget() {
    for (spent, held, required) in [(0.5, 0.4, 0.2), (0.999, 0.0, 0.08008)] {
        refusal(
            daily_budget(&json!(1), true, Some(spent), held, required).unwrap_err(),
            429,
            "budget_exceeded",
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
        "model",
    );
    refusal(
        daily_budget(&json!(1), true, None, 0.0, 0.1).unwrap_err(),
        503,
        "cloud_ledger_unavailable",
        "model",
    );
    for bad in [f64::NAN, f64::INFINITY, -1.0] {
        for (spent, held, required) in [(bad, 0.0, 0.1), (0.0, bad, 0.1), (0.0, 0.0, bad)] {
            refusal(
                daily_budget(&json!(1), true, Some(spent), held, required).unwrap_err(),
                503,
                "cloud_ledger_unavailable",
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
        "model",
    );
    refusal(Refusal::ledger(), 503, "cloud_ledger_unavailable", "model");
}
