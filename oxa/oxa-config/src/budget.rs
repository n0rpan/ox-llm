//! Owner budgets and what happens when one is exceeded (`oxa.onExceed`).
//!
//! The model can reach these limits by itself (it starts background jobs, spawns subagents,
//! replies without a tool call, spends tokens with no usage report). By default exceeding one
//! only warns and records it. The owner chooses a harder action per budget in the user-only
//! `oxa.onExceed` object, for example `{"bashJobs": "stop"}`; a project file cannot set it.

use crate::Settings;
use serde_json::Value;

/// A budget with an owner-chosen action when it is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Budget {
    /// `oxa.bash.maxJobs`: background jobs running at once.
    BashJobs,
    /// `oxa.subagent.maxChildren`: subagent children running or queued.
    SubagentChildren,
    /// A goal pursuing with no other budget that answers twice without a tool call.
    GoalIdle,
    /// A goal under a token cap whose request reported no token usage.
    GoalUnknownUsage,
}

/// What exceeding a budget does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnExceed {
    /// Show a warning and record it; the work goes on.
    Warn,
    /// A goal pauses (`/goal resume` continues). For a job or child budget it refuses, like `Stop`.
    Pause,
    /// A goal stops as blocked. For a job or child budget it refuses the over-cap call.
    Stop,
}

impl Budget {
    /// The field name inside `oxa.onExceed`.
    pub const fn key(self) -> &'static str {
        match self {
            Budget::BashJobs => "bashJobs",
            Budget::SubagentChildren => "subagentChildren",
            Budget::GoalIdle => "goalIdle",
            Budget::GoalUnknownUsage => "goalUnknownUsage",
        }
    }

    /// The action when the owner chose none.
    pub const fn default_action(self) -> OnExceed {
        match self {
            Budget::BashJobs | Budget::SubagentChildren => OnExceed::Warn,
            Budget::GoalIdle | Budget::GoalUnknownUsage => OnExceed::Pause,
        }
    }
}

impl OnExceed {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "warn" => Some(OnExceed::Warn),
            "pause" => Some(OnExceed::Pause),
            "stop" => Some(OnExceed::Stop),
            _ => None,
        }
    }

    /// True for any action that refuses or ends work instead of only warning.
    pub const fn enforces(self) -> bool {
        !matches!(self, OnExceed::Warn)
    }
}

impl Settings {
    /// The owner's action for `budget`: `oxa.onExceed.<name>` from the user file (a project
    /// file cannot set it), else the budget's default. An unknown value falls back to it.
    pub fn on_exceed(&self, budget: Budget) -> OnExceed {
        self.user_value("oxa.onExceed")
            .and_then(Value::as_object)
            .and_then(|map| map.get(budget.key()))
            .and_then(Value::as_str)
            .and_then(OnExceed::parse)
            .unwrap_or(budget.default_action())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_warn_for_caps_and_pause_for_goals() {
        let settings = Settings::default();
        assert_eq!(settings.on_exceed(Budget::BashJobs), OnExceed::Warn);
        assert_eq!(settings.on_exceed(Budget::SubagentChildren), OnExceed::Warn);
        assert_eq!(settings.on_exceed(Budget::GoalIdle), OnExceed::Pause);
        assert_eq!(settings.on_exceed(Budget::GoalUnknownUsage), OnExceed::Pause);
    }

    #[test]
    fn only_the_user_layer_chooses_and_bad_values_fall_back() {
        let user = serde_json::json!({"oxa": {"onExceed": {
            "bashJobs": "stop", "goalIdle": "warn", "goalUnknownUsage": "maybe"
        }}});
        let project = serde_json::json!({"oxa": {"onExceed": {"subagentChildren": "stop"}}});
        let settings = Settings::from_values(user, project);
        assert_eq!(settings.on_exceed(Budget::BashJobs), OnExceed::Stop);
        assert_eq!(settings.on_exceed(Budget::GoalIdle), OnExceed::Warn);
        assert_eq!(settings.on_exceed(Budget::GoalUnknownUsage), OnExceed::Pause);
        assert_eq!(settings.on_exceed(Budget::SubagentChildren), OnExceed::Warn);
    }

    /// #1918, #1965: a project file cannot turn `wait` back on, raise the output cap or the
    /// job and child caps.
    #[test]
    fn a_project_file_cannot_set_the_owner_caps() {
        let user = serde_json::json!({"oxa": {"tools": {"wait": false}}});
        let project = serde_json::json!({"oxa": {
            "tools": {"wait": true},
            "toolOutputCap": 4_000_000_000u64,
            "bash": {"maxJobs": 9999},
            "subagent": {"maxChildren": 999}
        }});
        let settings = Settings::from_values(user, project);
        assert!(!settings.bool("oxa.tools.wait", true));
        assert_eq!(settings.get("oxa.toolOutputCap"), None);
        assert_eq!(settings.get("oxa.bash.maxJobs"), None);
        assert_eq!(settings.get("oxa.subagent.maxChildren"), None);
    }
}
