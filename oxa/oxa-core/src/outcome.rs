//! Terminal outcomes for unattended callers. Error classes never contain diagnostic text.
use crate::RunEnd;
use oxa_proto::{codes, OxaError, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunOutcome {
    Done,
    #[default]
    NothingToDo,
    Blocked(&'static str),
    Failed(&'static str),
    Interrupted,
    Stalled,
}

impl RunOutcome {
    pub fn from_error(error: &OxaError) -> Self {
        match error.code {
            codes::ABORTED => Self::Interrupted,
            codes::BUDGET_EXHAUSTED
            | codes::OFFLINE
            | "cloud_credentials_unavailable"
            | "cloud_ledger_unavailable"
            | "cloud_sdk_unavailable"
            | "cloud_quota_exhausted" => Self::Blocked(error.code),
            _ => Self::Failed(error.code),
        }
    }

    pub(crate) fn from_end(end: Option<&RunEnd>) -> Self {
        match end {
            Some(RunEnd::Stopped) => Self::Done,
            Some(RunEnd::Aborted) => Self::Interrupted,
            Some(RunEnd::Failed(error)) => Self::from_error(error),
            None => Self::NothingToDo,
        }
    }

    /// Keep the first unsuccessful outcome so a later prompt cannot hide lost work.
    pub fn include(&mut self, next: Self) {
        if matches!(self, Self::NothingToDo) || matches!(self, Self::Done) && next != Self::NothingToDo {
            *self = next;
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Done => 0,
            Self::NothingToDo => 4,
            Self::Interrupted => 130,
            Self::Stalled => 9,
            Self::Blocked(class) => match *class {
                codes::BUDGET_EXHAUSTED => oxa_proto::EXIT_BUDGET,
                "cloud_credentials_unavailable" => 5,
                "cloud_ledger_unavailable" => 6,
                codes::OFFLINE => 7,
                _ => 8,
            },
            Self::Failed(class) => match *class {
                codes::TERMINAL_CLOSED => 129,
                codes::USAGE | codes::UNSUPPORTED_IN_OXA | codes::ENGINE_DOWN => 2,
                _ => 1,
            },
        }
    }

    pub fn record(&self) -> Value {
        let (outcome, error_class) = match self {
            Self::Done => ("done", None),
            Self::NothingToDo => ("nothing_to_do", None),
            Self::Blocked(class) => ("blocked", Some(*class)),
            Self::Failed(class) => ("failed", Some(*class)),
            Self::Interrupted => ("interrupted", Some(codes::ABORTED)),
            Self::Stalled => ("stalled", Some("goal_stalled")),
        };
        serde_json::json!({"type":"oxa_result", "outcome":outcome,
            "error_class":error_class, "exit_code":self.exit_code()})
    }
}

#[cfg(test)]
mod terminal_loss_tests {
    use super::*;

    #[test]
    fn terminal_loss_has_a_distinct_machine_class_and_hangup_exit() {
        let outcome = RunOutcome::from_error(&OxaError::new(codes::TERMINAL_CLOSED, "closed"));
        assert_eq!(outcome, RunOutcome::Failed(codes::TERMINAL_CLOSED));
        assert_eq!(outcome.exit_code(), 129);
        assert_eq!(outcome.record()["error_class"], "terminal_closed");
        assert_eq!(outcome.record()["exit_code"], 129);
        assert_eq!(RunOutcome::Interrupted.exit_code(), 130);
    }
}
