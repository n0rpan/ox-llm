use serde::{Deserialize, Serialize};

/// One side effect, never a worker or batch lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Admit,
    Push,
    Approve,
    Enqueue,
    Update,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Scope {
    Repository,
    PullRequest(u64),
    Task(String),
    Branch(String),
    Host(String),
    Path(String),
}

/// Identity resolved by the trusted frontend, not tool JSON. None means checked absence.
/// Supply the current PR whenever one exists; its head is deliberately not an identity field.
#[derive(Clone, Copy, Debug, Default)]
pub struct Subject<'a> {
    pub pr: Option<u64>,
    pub task: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub host: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalReason {
    Held,
    Frozen,
    InvalidSubject,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub revision: u64,
    pub rule_id: Option<String>,
    pub reason: RefusalReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConstraintKind {
    Hold,
    Freeze,
}

pub(super) struct Constraint<'a> {
    pub rule_id: &'a str,
    pub scope: &'a Scope,
    pub kind: ConstraintKind,
}

impl Scope {
    fn matches(&self, subject: Subject<'_>) -> bool {
        match self {
            Self::Repository => true,
            Self::PullRequest(pr) => subject.pr == Some(*pr),
            Self::Task(task) => subject.task == Some(task.as_str()),
            Self::Branch(branch) => subject.branch == Some(branch.as_str()),
            Self::Host(host) => subject.host == Some(host.as_str()),
            Self::Path(_) => false, // Path selectors are policy data, never hold identities.
        }
    }
}

pub(super) fn check<'a>(
    revision: u64,
    constraints: impl IntoIterator<Item = Constraint<'a>>,
    action: Action,
    subject: Subject<'_>,
) -> Result<(), Refusal> {
    let valid = subject.pr != Some(0)
        && match action {
            Action::Admit => {
                subject.task.is_some_and(|id| !id.is_empty())
                    && subject.branch.is_some_and(|branch| !branch.is_empty())
            }
            Action::Push => subject.branch.is_some_and(|branch| !branch.is_empty()),
            Action::Approve | Action::Enqueue | Action::Update => subject.pr.is_some(),
        };
    if !valid {
        return Err(Refusal {
            revision,
            rule_id: None,
            reason: RefusalReason::InvalidSubject,
        });
    }
    for constraint in constraints {
        let reason = match constraint.kind {
            ConstraintKind::Hold => RefusalReason::Held,
            ConstraintKind::Freeze if matches!(action, Action::Approve | Action::Enqueue) => {
                RefusalReason::Frozen
            }
            ConstraintKind::Freeze => continue,
        };
        if constraint.scope.matches(subject) {
            return Err(Refusal {
                revision,
                rule_id: Some(constraint.rule_id.to_owned()),
                reason,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject() -> Subject<'static> {
        Subject {
            pr: Some(480),
            task: Some("task"),
            branch: Some("agent/task"),
            host: Some("helper"),
        }
    }

    #[test]
    fn a_pr_hold_refuses_every_effect_and_has_no_head_or_actor_bypass() {
        let scope = Scope::PullRequest(480);
        for action in [
            Action::Admit,
            Action::Push,
            Action::Approve,
            Action::Enqueue,
            Action::Update,
        ] {
            let result = check(
                7,
                [Constraint {
                    rule_id: "R-7",
                    scope: &scope,
                    kind: ConstraintKind::Hold,
                }],
                action,
                subject(),
            );
            assert_eq!(
                result,
                Err(Refusal {
                    revision: 7,
                    rule_id: Some("R-7".into()),
                    reason: RefusalReason::Held
                })
            );
        }
        let different = Subject {
            pr: Some(481),
            ..subject()
        };
        assert!(check(
            7,
            [Constraint {
                rule_id: "R-7",
                scope: &scope,
                kind: ConstraintKind::Hold
            }],
            Action::Update,
            different
        )
        .is_ok());
    }

    #[test]
    fn freeze_refuses_only_approval_and_enqueue() {
        let scope = Scope::Repository;
        for action in [
            Action::Admit,
            Action::Push,
            Action::Approve,
            Action::Enqueue,
            Action::Update,
        ] {
            let result = check(
                3,
                [Constraint {
                    rule_id: "R-3",
                    scope: &scope,
                    kind: ConstraintKind::Freeze,
                }],
                action,
                subject(),
            );
            assert_eq!(
                result.is_err(),
                matches!(action, Action::Approve | Action::Enqueue)
            );
        }
    }

    #[test]
    fn pre_publication_admission_honors_task_branch_and_host_holds() {
        let subject = Subject {
            pr: None,
            ..subject()
        };
        for scope in [
            Scope::Task("task".into()),
            Scope::Branch("agent/task".into()),
            Scope::Host("helper".into()),
        ] {
            assert!(check(
                4,
                [Constraint {
                    rule_id: "R-4",
                    scope: &scope,
                    kind: ConstraintKind::Hold
                }],
                Action::Admit,
                subject
            )
            .is_err());
        }
        for action in [Action::Approve, Action::Enqueue, Action::Update] {
            assert_eq!(
                check(4, [], action, subject).unwrap_err().reason,
                RefusalReason::InvalidSubject
            );
        }
    }
}
