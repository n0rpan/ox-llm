//! Pure discovery overlay and unpriced-model admission policy for #628.
//!
//! No fetching, settings reads, budget mutations or request-path wiring lives here.
use crate::pricing::Price;
use std::fmt;

/// Public metadata only. Prices must already have passed `Price::from_model`.
#[derive(Clone, Debug)]
pub struct Model {
    pub provider: String,
    pub id: String,
    pub context_window: Option<u32>,
    pub max_output: Option<u32>,
    pub price: Option<Price>,
}

/// Merge one provider's discovery result with shipped metadata. `None` means
/// discovery is unavailable (offline fallback); `Some(&[])` is an empty live
/// list, not a failure. Only live IDs are returned when discovery succeeded.
/// Live fields win, and absent fields fall back by exact provider and ID.
pub fn merge(discovered: Option<&[Model]>, shipped: &[Model]) -> Vec<Model> {
    discovered
        .unwrap_or(shipped)
        .iter()
        .map(|live| {
            let mut model = live.clone();
            if let Some(fallback) = shipped
                .iter()
                .find(|entry| entry.provider == live.provider && entry.id == live.id)
            {
                model.context_window = model.context_window.or(fallback.context_window);
                model.max_output = model.max_output.or(fallback.max_output);
                model.price = model.price.or_else(|| fallback.price.clone());
            }
            model
        })
        .collect()
}

/// The caller supplies the configured billing route, never a provider claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Billing {
    Metered,
    Subscription,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Unpriced;

impl Unpriced {
    /// Credential-free, single-sentence status text.
    pub fn reason(&self) -> &'static str {
        "This model has no known price, so the budget cannot be enforced unless the owner allows unpriced models."
    }
}

impl fmt::Display for Unpriced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for Unpriced {}

/// Explicitly free pricing counts as known; missing pricing never means free.
/// Subscription billing is exempt because it is not charged per token.
pub fn admit(model: &Model, allow_unpriced: bool, billing: Billing) -> Result<(), Unpriced> {
    if model.price.is_some() || allow_unpriced || billing == Billing::Subscription {
        Ok(())
    } else {
        Err(Unpriced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(provider: &str, id: &str, priced: bool) -> Model {
        Model {
            provider: provider.into(),
            id: id.into(),
            context_window: None,
            max_output: None,
            price: priced.then(|| {
                Price::from_model(&json!({
                    "pricing": {"input_per_million": 2, "output_per_million": 4}
                }))
                .unwrap()
            }),
        }
    }

    #[test]
    fn admission_table() {
        for priced in [false, true] {
            for allow_unpriced in [false, true] {
                for billing in [Billing::Metered, Billing::Subscription] {
                    let expected = if priced || allow_unpriced || billing == Billing::Subscription {
                        Ok(())
                    } else {
                        Err(Unpriced)
                    };
                    assert_eq!(
                        admit(&model("example", "new-model", priced), allow_unpriced, billing),
                        expected,
                        "priced={priced}, allow_unpriced={allow_unpriced}, billing={billing:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn explicit_free_is_known_but_invalid_price_is_not() {
        let mut candidate = model("example", "free-model", false);
        candidate.price = Price::from_model(&json!({"free": true}));
        assert!(candidate.price.is_some());
        assert_eq!(admit(&candidate, false, Billing::Metered), Ok(()));
        for raw in [
            json!({}),
            json!({"pricing": {"input_per_million": 1}}),
            json!({"pricing": {"input_per_million": -1, "output_per_million": 1}}),
        ] {
            candidate.price = Price::from_model(&raw);
            assert!(candidate.price.is_none());
            assert_eq!(admit(&candidate, false, Billing::Metered), Err(Unpriced));
        }
    }

    #[test]
    fn status_reason_explains_the_budget_gap() {
        assert_eq!(
            Unpriced.reason(),
            "This model has no known price, so the budget cannot be enforced unless the owner allows unpriced models."
        );
        assert_eq!(Unpriced.to_string(), Unpriced.reason());
    }

    #[test]
    fn shipped_fields_fill_live_gaps_before_admission() {
        let mut shipped = model("example", "known-model", true);
        shipped.context_window = Some(128_000);
        shipped.max_output = Some(8_000);
        let live = model("example", "known-model", false);
        let merged = merge(Some(&[live]), &[shipped]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].context_window, Some(128_000));
        assert_eq!(merged[0].max_output, Some(8_000));
        assert_eq!(admit(&merged[0], false, Billing::Metered), Ok(()));
    }

    #[test]
    fn live_fields_win_without_resurrecting_unlisted_ids() {
        let mut shipped = model("example", "known-model", true);
        shipped.context_window = Some(128_000);
        shipped.max_output = Some(8_000);
        let mut live = model("example", "known-model", false);
        live.context_window = Some(256_000);
        live.max_output = Some(16_000);
        live.price = Price::from_model(&json!({"free": true}));
        let merged = merge(
            Some(&[live.clone(), model("example", "new-model", false)]),
            &[shipped, model("example", "removed-model", true)],
        );
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].context_window, live.context_window);
        assert_eq!(merged[0].max_output, live.max_output);
        assert_eq!(
            merged[0].price.as_ref().unwrap().normalized(),
            live.price.as_ref().unwrap().normalized()
        );
        assert_eq!(admit(&merged[1], false, Billing::Metered), Err(Unpriced));
    }

    #[test]
    fn metadata_never_crosses_provider_or_model_boundaries() {
        let live = [model("example", "new-model", false)];
        let shipped = [
            model("other-provider", "new-model", true),
            model("example", "NEW-model", true),
        ];
        let merged = merge(Some(&live), &shipped);
        assert_eq!(merged.len(), 1);
        assert_eq!(admit(&merged[0], false, Billing::Metered), Err(Unpriced));
    }

    #[test]
    fn unavailable_discovery_falls_back_but_empty_discovery_does_not() {
        let shipped = [model("example", "known-model", true)];
        let fallback = merge(None, &shipped);
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0].id, "known-model");
        assert_eq!(admit(&fallback[0], false, Billing::Metered), Ok(()));
        assert!(merge(Some(&[]), &shipped).is_empty());
        assert!(merge(None, &[]).is_empty());
    }
}
