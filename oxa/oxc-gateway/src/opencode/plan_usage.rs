use serde_json::Value;

/// The three independent windows reported by the Go account. Missing or
/// changed fields stay unknown and grant no admission.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanUsage {
    pub rolling: Option<PlanWindow>,
    pub weekly: Option<PlanWindow>,
    pub monthly: Option<PlanWindow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanWindow {
    pub used_percent: Option<f64>,
    /// Provider-reported reset as Unix seconds.
    pub resets_at: Option<u64>,
}

pub(super) fn parse(body: &Value) -> Option<PlanUsage> {
    let window = |name| {
        let fields = body["usage"].get(name)?.as_object()?;
        let used_percent = fields
            .get("percent")
            .and_then(Value::as_f64)
            .filter(|value| (0.0..=100.0).contains(value));
        let resets_at = fields
            .get("resetsAt")
            .and_then(Value::as_str)
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .and_then(|value| value.timestamp().try_into().ok());
        (used_percent.is_some() || resets_at.is_some()).then_some(PlanWindow {
            used_percent,
            resets_at,
        })
    };
    let usage = PlanUsage {
        rolling: window("rolling"),
        weekly: window("weekly"),
        monthly: window("monthly"),
    };
    (usage.rolling.is_some() || usage.weekly.is_some() || usage.monthly.is_some()).then_some(usage)
}
