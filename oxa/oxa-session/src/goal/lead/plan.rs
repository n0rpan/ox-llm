//! Acceptance and evidence for an owner goal and its immediate sub-goals.
use super::{atom, fields, invalid, list, number, string, text};
use oxa_proto::{work::MAX_ITEMS, OxaError, Value};
use serde_json::json;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalPlan {
    pub acceptance: Vec<GoalCheck>,
    pub subgoals: Vec<Subgoal>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subgoal {
    pub id: String,
    pub objective: String,
    pub acceptance: Vec<GoalCheck>,
    pub board_items: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalCheck {
    pub id: String,
    pub acceptance: String,
    /// Key of the trusted done check. A Board item is never a check result.
    pub done_check: String,
    pub result: Option<CheckResult>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckResult {
    pub passed: bool,
    pub receipt: String,
    pub observed_at_ms: u64,
}

impl GoalPlan {
    /// Work is ready for owner-goal completion only with evidence for every check.
    pub fn verified(&self) -> bool {
        self.validate().is_ok()
            && passed(&self.acceptance)
            && self.subgoals.iter().all(|subgoal| passed(&subgoal.acceptance))
    }

    /// Failed done checks take precedence when choosing the next sub-goal to re-plan.
    pub fn next_subgoal(&self) -> Option<&Subgoal> {
        self.subgoals
            .iter()
            .find(|subgoal| {
                subgoal
                    .acceptance
                    .iter()
                    .any(|check| check.result.as_ref().is_some_and(|result| !result.passed))
            })
            .or_else(|| self.subgoals.iter().find(|subgoal| !passed(&subgoal.acceptance)))
    }

    pub fn next_owner_check(&self) -> Option<&GoalCheck> {
        self.acceptance
            .iter()
            .find(|check| check.result.as_ref().is_some_and(|result| !result.passed))
            .or_else(|| self.acceptance.iter().find(|check| check.result.is_none()))
    }

    pub(in crate::goal) fn validate(&self) -> Result<(), OxaError> {
        const MAX_SUBGOALS: usize = 32;
        let mut subgoals = BTreeSet::new();
        let mut items = BTreeSet::new();
        if self.subgoals.is_empty()
            || self.subgoals.len() > MAX_SUBGOALS
            || !valid_checks(&self.acceptance)
            || self.subgoals.iter().any(|subgoal| {
                !atom(&subgoal.id)
                    || !subgoals.insert(&subgoal.id)
                    || !text(&subgoal.objective, 512)
                    || !valid_checks(&subgoal.acceptance)
                    || subgoal.board_items.len() > MAX_ITEMS
                    || subgoal
                        .board_items
                        .iter()
                        .any(|item| *item == 0 || !items.insert(*item))
            })
            || items.len() > MAX_ITEMS
            || self.acceptance.len() + self.subgoals.iter().map(|s| s.acceptance.len()).sum::<usize>() > 32
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub(in crate::goal) fn value(&self) -> Value {
        let subgoals: Vec<_> = self
            .subgoals
            .iter()
            .map(|subgoal| {
                json!({"id": subgoal.id, "objective": subgoal.objective,
                "acceptance": encode_checks(&subgoal.acceptance), "board_items": subgoal.board_items})
            })
            .collect();
        json!({"acceptance": encode_checks(&self.acceptance), "subgoals": subgoals})
    }

    pub(in crate::goal) fn decode(value: &Value) -> Result<Self, OxaError> {
        fields(value, &["acceptance", "subgoals"])?;
        let plan = Self {
            acceptance: list(&value["acceptance"], GoalCheck::decode)?,
            subgoals: list(&value["subgoals"], |subgoal| {
                fields(subgoal, &["id", "objective", "acceptance", "board_items"])?;
                Ok(Subgoal {
                    id: string(&subgoal["id"])?,
                    objective: string(&subgoal["objective"])?,
                    acceptance: list(&subgoal["acceptance"], GoalCheck::decode)?,
                    board_items: list(&subgoal["board_items"], number)?,
                })
            })?,
        };
        Ok(plan)
    }
}

fn passed(checks: &[GoalCheck]) -> bool {
    !checks.is_empty()
        && checks
            .iter()
            .all(|check| check.result.as_ref().is_some_and(|result| result.passed))
}

fn valid_checks(checks: &[GoalCheck]) -> bool {
    let mut ids = BTreeSet::new();
    !checks.is_empty()
        && checks.len() <= 8
        && checks.iter().all(|check| {
            atom(&check.id)
                && ids.insert(&check.id)
                && text(&check.acceptance, 512)
                && atom(&check.done_check)
                && check
                    .result
                    .as_ref()
                    .is_none_or(|result| result.observed_at_ms > 0 && text(&result.receipt, 512))
        })
}

fn encode_checks(checks: &[GoalCheck]) -> Vec<Value> {
    checks.iter().map(|check| {
        let result = check.result.as_ref().map(|result| json!({
            "passed": result.passed, "receipt": result.receipt, "observed_at_ms": result.observed_at_ms
        }));
        json!({"id": check.id, "acceptance": check.acceptance, "done_check": check.done_check, "result": result})
    }).collect()
}

impl GoalCheck {
    fn decode(check: &Value) -> Result<Self, OxaError> {
        fields(check, &["id", "acceptance", "done_check", "result"])?;
        let result = &check["result"];
        Ok(Self {
            id: string(&check["id"])?,
            acceptance: string(&check["acceptance"])?,
            done_check: string(&check["done_check"])?,
            result: if result.is_null() {
                None
            } else {
                fields(result, &["passed", "receipt", "observed_at_ms"])?;
                Some(CheckResult {
                    passed: result["passed"].as_bool().ok_or_else(invalid)?,
                    receipt: string(&result["receipt"])?,
                    observed_at_ms: number(&result["observed_at_ms"])?,
                })
            },
        })
    }
}
