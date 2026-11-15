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
}
