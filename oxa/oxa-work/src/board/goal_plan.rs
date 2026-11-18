//! Select the next goal step from one observed Board without reserving work.
use super::{Board, Entry};
use oxa_session::{GoalCheck, GoalPlan, Subgoal};

pub enum GoalStep<'a> {
    Check {
        subgoal: Option<&'a Subgoal>,
        check: &'a GoalCheck,
    },
    Work {
        subgoal: &'a Subgoal,
        item: &'a Entry,
    },
    Verified,
}

impl Board {
    /// Select within the current sub-goal. A failed check is a re-plan request,
    /// and a missing/closed Board item still requires its independent done check.
    pub fn next_goal_step<'a>(&'a self, plan: &'a GoalPlan) -> Option<GoalStep<'a>> {
        let Some(subgoal) = plan.next_subgoal() else {
            return plan
                .next_owner_check()
                .map(|check| GoalStep::Check { subgoal: None, check })
                .or_else(|| plan.verified().then_some(GoalStep::Verified));
        };
        let check = subgoal
            .acceptance
            .iter()
            .find(|check| check.result.as_ref().is_some_and(|result| !result.passed))
            .or_else(|| subgoal.acceptance.iter().find(|check| check.result.is_none()))?;
        if check.result.is_none() {
            if let Some(item) = self
                .entries
                .iter()
                .filter(|item| item.eligible && subgoal.board_items.contains(&item.number))
                .min_by_key(|item| item.priority())
            {
                return Some(GoalStep::Work { subgoal, item });
            }
        }
        Some(GoalStep::Check {
            subgoal: Some(subgoal),
            check,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Action;
    use oxa_session::CheckResult;

    #[test]
    fn goal_steps_use_linked_work_and_recheck_acceptance_after_it_leaves_the_board() {
        let check = GoalCheck {
            id: "install".into(),
            acceptance: "Artifact installs.".into(),
            done_check: "artifact.install".into(),
            result: None,
        };
        let mut plan = GoalPlan {
            acceptance: vec![check.clone()],
            subgoals: vec![Subgoal {
                id: "package".into(),
                objective: "Qualify package.".into(),
                acceptance: vec![check],
                board_items: vec![3, 4],
            }],
        };
        let mut board = Board {
            observed_at: 10,
            entries: [1, 4, 3]
                .map(|number| Entry {
                    number,
                    head: "a".repeat(40),
                    release_pinned: false,
                    action: Action::Ready,
                    lease: None,
                    attempts_today: 0,
                    eligible: true,
                })
                .to_vec(),
        };
        assert!(matches!(board.next_goal_step(&plan), Some(GoalStep::Work { item, .. }) if item.number == 3));
        let failure = Some(CheckResult {
            passed: false,
            receipt: "run:10".into(),
            observed_at_ms: 10_000,
        });
        plan.subgoals[0].acceptance[0].result = failure.clone();
        assert!(
            matches!(board.next_goal_step(&plan), Some(GoalStep::Check { check, .. }) if check.result == failure)
        );
        plan.subgoals[0].acceptance[0].result = None;
        board.entries.retain(|entry| entry.number == 1);
        assert!(matches!(
            board.next_goal_step(&plan),
            Some(GoalStep::Check { subgoal: Some(_), .. })
        ));
    }
}
