//! Pool preference over eligible accounting observations from the native gateway.
use crate::Result;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Strategy {
    #[default]
    ExpiringFirst,
    Ordered,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    order: Vec<String>,
    #[serde(default)]
    strategy: Strategy,
    lookahead_seconds: Option<u64>,
}

#[derive(Deserialize)]
struct Document {
    schema_version: u32,
    revision: u64,
    #[serde(default)]
    enabled: bool,
    selection: Option<Selection>,
}

pub struct PoolPolicy {
    revision: u64,
    selection: Selection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Preference {
    EarlierExpiry,
    OwnerOrder,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreferredPool<'a> {
    pub id: &'a str,
    pub policy_revision: u64,
    pub preference: Preference,
}

impl PoolPolicy {
    /// Decode the selection fields from the latest protected owner board
    /// document. The caller owns authorization and revision-checked writes.
    pub fn from_owner_document(document: &Value) -> Result<Option<Self>> {
        let Some(raw) = document.get("budget_pools") else {
            return Ok(None);
        };
        let Document {
            schema_version,
            revision,
            enabled,
            selection,
        } = Document::deserialize(raw).map_err(|_| "invalid owner budget pool policy")?;
        if schema_version != 1 {
            return Err("unsupported owner budget pool policy version".into());
        }
        if !enabled {
            return Ok(None);
        }
        let selection = selection.ok_or("owner pool selection is missing")?;
        let mut seen = BTreeSet::new();
        if selection
            .order
            .iter()
            .any(|id| id.trim().is_empty() || !seen.insert(id))
        {
            return Err("owner pool order contains an empty or duplicate ID".into());
        }
        Ok(Some(Self { revision, selection }))
    }

    /// `eligible` contains only configured pools allowed for the task kind,
    /// with fresh observations and room. Its values are the expiry of usable
    /// capacity, or `None` for capacity that has no expiry. The gateway still
    /// reserves and checks every applicable cap before forwarding.
    pub fn prefer(&self, now: u64, eligible: &BTreeMap<&str, Option<u64>>) -> Option<PreferredPool<'_>> {
        let horizon = self
            .selection
            .lookahead_seconds
            .map_or(u64::MAX, |seconds| now.saturating_add(seconds));
        let (id, upcoming_expiry) = self
            .selection
            .order
            .iter()
            .filter_map(|id| {
                let expiry = *eligible.get(id.as_str())?;
                if expiry.is_some_and(|reset| reset <= now) {
                    return None;
                }
                let upcoming = expiry
                    .filter(|reset| self.selection.strategy == Strategy::ExpiringFirst && *reset <= horizon);
                Some((id.as_str(), upcoming))
            })
            .min_by_key(|(_, upcoming)| (upcoming.is_none(), *upcoming))?;
        Some(PreferredPool {
            id,
            policy_revision: self.revision,
            preference: if upcoming_expiry.is_some() {
                Preference::EarlierExpiry
            } else {
                Preference::OwnerOrder
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn document(revision: u64, selection: Value) -> Value {
        json!({"budget_pools":{
            "schema_version":1,"revision":revision,"enabled":true,"selection":selection
        }})
    }

    #[test]
    fn prefers_expiring_capacity_only_in_the_configured_horizon() {
        let config = document(
            1,
            json!({"order":["paid","monthly","plan","closed"],"lookahead_seconds":30}),
        );
        let policy = PoolPolicy::from_owner_document(&config).unwrap().unwrap();
        let capacity = BTreeMap::from([
            ("paid", None),
            ("monthly", Some(112)),
            ("plan", Some(118)),
            ("closed", Some(99)),
            ("unlisted", Some(101)),
        ]);
        assert_eq!(
            policy.prefer(100, &capacity),
            Some(PreferredPool {
                id: "monthly",
                policy_revision: 1,
                preference: Preference::EarlierExpiry,
            })
        );
        assert_eq!(
            policy
                .prefer(70, &BTreeMap::from([("paid", None), ("plan", Some(118))]))
                .unwrap()
                .id,
            "paid"
        );
        assert_eq!(policy.prefer(120, &capacity).unwrap().id, "paid");
    }

    #[test]
    fn current_owner_revision_controls_order_and_ties() {
        let config = document(8, json!({"order":["paid","plan","credit"],"strategy":"ordered"}));
        let available = BTreeMap::from([("paid", None), ("plan", Some(90)), ("credit", Some(90))]);
        let before = PoolPolicy::from_owner_document(&config).unwrap().unwrap();
        assert_eq!(before.prefer(30, &available).unwrap().id, "paid");
        let latest = document(
            9,
            json!({"order":["paid","credit","plan"],"strategy":"expiring_first"}),
        );
        let after = PoolPolicy::from_owner_document(&latest).unwrap().unwrap();
        assert_eq!(
            after.prefer(30, &available),
            Some(PreferredPool {
                id: "credit",
                policy_revision: 9,
                preference: Preference::EarlierExpiry,
            })
        );
        assert!(after.prefer(30, &BTreeMap::from([("unknown", None)])).is_none());
    }

    #[test]
    fn absent_disabled_and_invalid_policy_cannot_make_a_selection() {
        assert!(PoolPolicy::from_owner_document(&json!({})).unwrap().is_none());
        assert!(PoolPolicy::from_owner_document(&json!({"budget_pools":{
            "schema_version":1,"revision":0,"enabled":false
        }}))
        .unwrap()
        .is_none());
        for selection in [
            json!({"order":["same","same"]}),
            json!({"order":["  "]}),
            json!({"order":["paid"],"lookahead_seconds":-1}),
            json!({"order":["paid"],"strategy":"unknown"}),
        ] {
            assert!(PoolPolicy::from_owner_document(&document(1, selection)).is_err());
        }
    }
}
