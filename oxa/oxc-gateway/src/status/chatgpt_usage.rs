//! Pure, best-effort decoding of the private, unversioned `wham/usage` reply.
//! Missing or mistyped fields stay unknown; no HTTP, policy, or inferred values.
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub plan_type: Option<String>,
    pub primary: Option<Window>,
    pub secondary: Option<Window>,
    pub credits: Option<Credits>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Window {
    pub used_percent: Option<f64>,
    /// Provider-reported Unix seconds, not a locally calculated deadline.
    pub reset_at: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credits {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    /// The provider's decimal string, preserved without rounding or guessing units.
    pub balance: Option<String>,
}

pub fn parse(body: &Value) -> Usage {
    let limits = body.get("rate_limit");
    Usage {
        plan_type: body.get("plan_type").and_then(Value::as_str).map(str::to_owned),
        primary: limits.and_then(|v| v.get("primary_window")).and_then(window),
        secondary: limits.and_then(|v| v.get("secondary_window")).and_then(window),
        credits: body
            .get("credits")
            .and_then(Value::as_object)
            .map(|fields| Credits {
                has_credits: fields.get("has_credits").and_then(Value::as_bool),
                unlimited: fields.get("unlimited").and_then(Value::as_bool),
                balance: fields.get("balance").and_then(Value::as_str).map(str::to_owned),
            }),
    }
}

fn window(value: &Value) -> Option<Window> {
    let fields = value.as_object()?;
    Some(Window {
        used_percent: fields
            .get("used_percent")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite()),
        reset_at: fields.get("reset_at").and_then(Value::as_u64),
    })
}
