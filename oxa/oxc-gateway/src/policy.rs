//! Stateless remote refusal contracts. All facts come from trusted gateway
//! components, never from request-supplied admission or credential flags.
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub status: u16,
    pub message: &'static str,
    pub code: &'static str,
    pub param: &'static str,
}
impl Refusal {
    pub fn credentials() -> Self {
        Self {
            status: 503,
            message: "Cloud credentials are not configured.",
            code: "cloud_credentials_unavailable",
            param: "model",
        }
    }
    pub fn ledger() -> Self {
        Self {
            status: 503,
            message: "The request ledger is unreadable; cloud calls are blocked until it is repaired.",
            code: "cloud_ledger_unavailable",
            param: "model",
        }
    }
    /// A configured cap cannot be verified because an earlier unbudgeted request has no known cost.
    pub fn cost_unknown_history() -> Self {
        Self {
            status: 429,
            message: "The daily cloud budget cannot be checked: earlier requests today have an unknown cost. Unset the budget or wait for the next spend day.",
            code: "cost_unknown_history",
            param: "model",
        }
    }
    pub fn body(&self) -> Value {
        let kind = match self.status {
            403 => "permission_error",
            429 => "rate_limit_error",
            _ => "cloud_error",
        };
        json!({"error":{"message":self.message,"type":kind,"code":self.code,"param":self.param}})
    }
}

/// Run before reading credentials, reserving spend or contacting a provider.
/// This does not acquire local engine, memory or inference-lease resources.
pub fn before_credentials(
    payload: &Value,
    shared: bool,
    offline: bool,
    pricing_known: bool,
    capped: bool,
) -> Result<(), Refusal> {
    if shared {
        return Err(Refusal {
            status: 403,
            message: "Cloud models are not available to LAN-share requests.",
            code: "cloud_not_shared",
            param: "model",
        });
    }
    if offline {
        return Err(Refusal {
            status: 403,
            message: "Cloud models are unavailable in offline mode.",
            code: "offline",
            param: "model",
        });
    }
    if payload
        .get("n")
        .is_some_and(|n| !n.is_null() && n.as_u64() != Some(1))
    {
        return Err(Refusal {
            status: 400,
            message: "Cloud requests support one completion.",
            code: "invalid_value",
            param: "n",
        });
    }
    // With no daily cap there is no budget check, so a model with no known price is admitted and
    // its usage is recorded with unknown cost. Only a configured cap needs a price to bound spend.
    if !pricing_known && capped {
        return Err(Refusal {
            status: 400,
            message: "With a daily budget set, cloud models need catalog pricing or explicit free: true.",
            code: "cloud_pricing_unavailable",
            param: "pricing",
        });
    }
    Ok(())
}

fn ledger_disabled() -> Refusal {
    Refusal {
        status: 503,
        message: "The request ledger is disabled; cloud calls are blocked until it is enabled.",
        code: "cloud_ledger_unavailable",
        param: "model",
    }
}

/// Admission math only. An unset cap (`null`) means no budget check: the owner
/// chose no limit, so the request is admitted and its cost is still recorded. The accounting slice supplies an atomic ledger snapshot
/// and outstanding holds, and retains the lock through creation of a new hold.
/// This function alone neither grants a reservation nor forwards a request.
pub fn daily_budget(
    cap: &Value,
    ledger_enabled: bool,
    spent: Option<f64>,
    reserved: f64,
    required: f64,
) -> Result<(), Refusal> {
    if cap.is_null() {
        // Only the cap check is skipped. Metered admission still needs the durable ledger.
        return if ledger_enabled {
            Ok(())
        } else {
            Err(ledger_disabled())
        };
    }
    let Some(cap) = cap.as_f64().filter(|n| n.is_finite() && *n > 0.0) else {
        // A configured but unusable cap (zero, negative, non-numeric) is a
        // configuration error, not an absent budget.
        return Err(Refusal {
            status: 403,
            message: "`cloud.daily_budget_usd` must be a positive number when set.",
            code: "budget_not_configured",
            param: "model",
        });
    };
    if !ledger_enabled {
        return Err(ledger_disabled());
    }
    let spent = spent
        .filter(|n| n.is_finite() && *n >= 0.0)
        .ok_or_else(Refusal::ledger)?;
    if !reserved.is_finite() || reserved < 0.0 || !required.is_finite() || required < 0.0 {
        return Err(Refusal::ledger());
    }
    if spent + reserved >= cap || spent + reserved + required > cap + 1e-12 {
        return Err(Refusal {
            status: 429,
            message: "The daily cloud budget is exhausted.",
            code: "budget_exceeded",
            param: "model",
        });
    }
    Ok(())
}
