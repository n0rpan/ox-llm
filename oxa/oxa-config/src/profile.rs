//! The T5 `oxa.thinking.policy` and the defaults of oxa's tools and context handling: typed
//! access to the settings that shape the preamble, compaction, prefill and the built-in tools.
//!
//! Every default here is unconditional. A `null` or unset value takes the value below; an
//! explicit setting in any layer wins.
use crate::Settings;

/// T5 `oxa.thinking.policy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingPolicy {
    /// The session's thinking level applies to every request.
    Fixed,
    /// Thinking on for the first reply after a user prompt, off for every other request; the
    /// level `off` always wins. The default.
    #[default]
    Auto,
}

impl ThinkingPolicy {
    pub const SETTING: &'static str = "oxa.thinking.policy";
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingPolicy::Fixed => "fixed",
            ThinkingPolicy::Auto => "auto",
        }
    }
    pub fn parse(s: &str) -> Option<ThinkingPolicy> {
        match s {
            "fixed" => Some(ThinkingPolicy::Fixed),
            "auto" => Some(ThinkingPolicy::Auto),
            _ => None,
        }
    }
}

/// Soft length target of a summary, in tokens.
pub const SUMMARY_TARGET_DEFAULT: u32 = 700;
/// `bash` timeout in seconds when the call sets none.
pub const BASH_TIMEOUT_DEFAULT: u32 = 120;
/// The default of `oxa.bash.maxJobs`: background jobs that may run at once.
pub const BASH_MAX_JOBS_DEFAULT: u32 = 16;

impl Settings {
    /// `oxa.thinking.policy` as set by a layer; `None` = unset (the default decides).
    pub fn thinking_policy_setting(&self) -> Option<ThinkingPolicy> {
        self.string(ThinkingPolicy::SETTING)
            .as_deref()
            .and_then(ThinkingPolicy::parse)
    }
    /// The policy in force: an explicit setting wins, else [`ThinkingPolicy::Auto`].
    pub fn thinking_policy(&self) -> ThinkingPolicy {
        self.thinking_policy_setting().unwrap_or_default()
    }

    /// Optional local `oxa.thinking.budget` in generated reasoning text tokens. No default cap.
    pub fn thinking_budget(&self) -> Option<u64> {
        self.get("oxa.thinking.budget")
            .and_then(serde_json::Value::as_u64)
    }

    /// A `null`-default bool that is on unless a layer sets it off.
    fn default_bool(&self, key: &str) -> bool {
        self.bool(key, true)
    }
    /// `oxa.diet.output` (V020-OPTIMIZE #1): the output rules of the preamble.
    pub fn diet_output(&self) -> bool {
        self.default_bool("oxa.diet.output")
    }
    /// `oxa.diet.tools` (V020-OPTIMIZE #2): lossless shaping of tool results.
    pub fn diet_tools(&self) -> bool {
        self.default_bool("oxa.diet.tools")
    }
    /// `oxa.compaction.idleSummary` (V020-OPTIMIZE #7): the summary is computed ahead in idle
    /// time.
    pub fn idle_summary(&self) -> bool {
        self.default_bool("oxa.compaction.idleSummary")
    }
    /// `oxa.compaction.turnStart`: compact before a prompt is sent when the context is already
    /// above the compaction point (for example after an aborted turn). Unset = on.
    pub fn turn_start_compaction(&self) -> bool {
        self.default_bool("oxa.compaction.turnStart")
    }
    /// `oxa.compaction.midRun`: compact between tool steps of a run when the context passes the
    /// compaction point. Unset = on.
    pub fn mid_run_compaction(&self) -> bool {
        self.default_bool("oxa.compaction.midRun")
    }
    /// `oxa.prefill.paste` (V020-OPTIMIZE #13).
    pub fn prefill_paste(&self) -> bool {
        self.default_bool("oxa.prefill.paste")
    }
    /// `oxa.prefill.tools` (OXA-LOCAL-FIRST F1).
    pub fn prefill_tools(&self) -> bool {
        self.default_bool("oxa.prefill.tools")
    }
    /// `oxa.compaction.summaryTarget` in tokens (V020-OPTIMIZE #7); 0 = no target. Unset =
    /// [`SUMMARY_TARGET_DEFAULT`].
    pub fn summary_target(&self) -> u32 {
        self.u32("oxa.compaction.summaryTarget", SUMMARY_TARGET_DEFAULT)
    }
    /// `oxa.bash.defaultTimeout` in seconds (F5); 0 = none. Unset = [`BASH_TIMEOUT_DEFAULT`].
    pub fn bash_default_timeout(&self) -> u32 {
        self.u32("oxa.bash.defaultTimeout", BASH_TIMEOUT_DEFAULT)
    }

    /// `oxa.bash.maxJobs`: how many background jobs may run at once. Unset = 16; at least 1.
    pub fn bash_max_jobs(&self) -> usize {
        (self.u32("oxa.bash.maxJobs", BASH_MAX_JOBS_DEFAULT) as usize).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;
    use serde_json::json;

    #[test]
    fn thinking_policy_defaults_to_auto_and_the_setting_wins() {
        let none = Settings::from_values(json!({}), json!({}));
        assert_eq!(none.thinking_policy(), ThinkingPolicy::Auto);
        let auto = Settings::from_values(json!({"oxa": {"thinking": {"policy": "auto"}}}), json!({}));
        assert_eq!(auto.thinking_policy(), ThinkingPolicy::Auto);
        let fixed = Settings::from_values(json!({"oxa": {"thinking": {"policy": "fixed"}}}), json!({}));
        assert_eq!(fixed.thinking_policy(), ThinkingPolicy::Fixed);
        let bad = Settings::from_values(json!({"oxa": {"thinking": {"policy": "on"}}}), json!({}));
        assert_eq!(bad.thinking_policy(), ThinkingPolicy::Auto);
        assert!(bad
            .diagnostics()
            .iter()
            .any(|d| d.message.contains("\"fixed\", \"auto\"")));
        let pol = schema()
            .iter()
            .find(|d| d.key == ThinkingPolicy::SETTING)
            .unwrap();
        assert_eq!(pol.default, "null");
    }

    #[test]
    fn optional_thinking_budget_has_no_default_and_keeps_zero() {
        assert_eq!(
            Settings::from_values(json!({}), json!({})).thinking_budget(),
            None
        );
        for value in [json!(null), json!(0), json!(512), json!(u64::MAX)] {
            let settings =
                Settings::from_values(json!({"oxa":{"thinking":{"budget":value.clone()}}}), json!({}));
            assert!(settings.diagnostics().is_empty(), "{:?}", settings.diagnostics());
            assert_eq!(settings.thinking_budget(), value.as_u64());
        }
        let cleared = Settings::from_values(
            json!({"oxa":{"thinking":{"budget":512}}}),
            json!({"oxa":{"thinking":{"budget":null}}}),
        );
        assert!(cleared.diagnostics().is_empty());
        assert_eq!(cleared.thinking_budget(), None);
        for value in [json!(-1), json!(1.5), json!(true), json!("512")] {
            let settings = Settings::from_values(json!({"oxa":{"thinking":{"budget":value}}}), json!({}));
            assert!(!settings.diagnostics().is_empty());
            assert_eq!(settings.thinking_budget(), None);
        }
    }

    #[test]
    fn the_defaults_are_on_and_the_settings_win() {
        let none = Settings::from_values(json!({}), json!({}));
        assert!(none.diet_output());
        assert!(none.diet_tools());
        assert!(none.idle_summary());
        assert!(none.turn_start_compaction());
        assert!(none.mid_run_compaction());
        assert!(none.prefill_paste());
        assert!(none.prefill_tools());
        assert_eq!(none.summary_target(), SUMMARY_TARGET_DEFAULT);
        assert_eq!(none.bash_default_timeout(), BASH_TIMEOUT_DEFAULT);
        assert_eq!(none.bash_max_jobs(), 16);

        let off = Settings::from_values(
            json!({"oxa": {
                "diet": {"output": false, "tools": false},
                "compaction": {"idleSummary": false, "turnStart": false, "midRun": false, "summaryTarget": 0},
                "prefill": {"paste": false, "tools": false},
                "bash": {"defaultTimeout": 0}
            }}),
            json!({}),
        );
        assert!(!off.diet_output());
        assert!(!off.diet_tools());
        assert!(!off.idle_summary());
        assert!(!off.turn_start_compaction());
        assert!(!off.mid_run_compaction());
        assert!(!off.prefill_paste());
        assert!(!off.prefill_tools());
        assert_eq!(off.summary_target(), 0);
        assert_eq!(off.bash_default_timeout(), 0);
    }

    /// `oxa.bash.maxJobs` reads the owner's setting; zero is refused as one job.
    #[test]
    fn jobs_limit_from_setting() {
        let set = Settings::from_values(json!({"oxa": {"bash": {"maxJobs": 3}}}), json!({}));
        assert_eq!(set.bash_max_jobs(), 3);
        let zero = Settings::from_values(json!({"oxa": {"bash": {"maxJobs": 0}}}), json!({}));
        assert_eq!(zero.bash_max_jobs(), 1);
        let unset = Settings::from_values(json!({}), json!({}));
        assert_eq!(unset.bash_max_jobs(), 16);
    }
}
