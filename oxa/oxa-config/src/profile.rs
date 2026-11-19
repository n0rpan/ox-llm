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
/// `oxa.bash.waitCap` default: the longest a job read blocks, in seconds.
pub const BASH_WAIT_CAP_DEFAULT: u32 = 300;
/// The default of `oxa.bash.maxJobs`: background jobs that may run at once.
pub const BASH_MAX_JOBS_DEFAULT: u32 = 16;
/// The default of `oxa.read.maxImageBytes`: 20 MiB per image.
pub const READ_MAX_IMAGE_BYTES_DEFAULT: u32 = 20 * 1024 * 1024;

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

    /// `oxa.bash.waitCap` in seconds: the longest a bash job read blocks. Unset = 300; at least 1.
    pub fn bash_wait_cap(&self) -> u32 {
        self.user_u32("oxa.bash.waitCap", BASH_WAIT_CAP_DEFAULT).max(1)
    }

    /// `oxa.bash.maxJobs`: how many background jobs may run at once. Unset = 16; at least 1.
    pub fn bash_max_jobs(&self) -> usize {
        (self.user_u32("oxa.bash.maxJobs", BASH_MAX_JOBS_DEFAULT) as usize).max(1)
    }

    /// `oxa.read.maxImageBytes` (B2b): the largest image `read` sends. Unset = 20 MiB; 0 = no limit.
    pub fn read_max_image_bytes(&self) -> u32 {
        self.user_u32("oxa.read.maxImageBytes", READ_MAX_IMAGE_BYTES_DEFAULT)
    }
}

/// `compaction.profile`: which compaction values a model runs with (issue #1594).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactionProfile {
    /// `cloud` for a cloud model, `local` otherwise. The default.
    #[default]
    Auto,
    /// The values oxa has always used.
    Local,
    /// A larger verbatim tail and a later compaction point, for a model with a large window.
    Cloud,
}

impl CompactionProfile {
    pub const SETTING: &'static str = "compaction.profile";
    pub fn as_str(self) -> &'static str {
        match self {
            CompactionProfile::Auto => "auto",
            CompactionProfile::Local => "local",
            CompactionProfile::Cloud => "cloud",
        }
    }
    pub fn parse(s: &str) -> Option<CompactionProfile> {
        match s {
            "auto" => Some(CompactionProfile::Auto),
            "local" => Some(CompactionProfile::Local),
            "cloud" => Some(CompactionProfile::Cloud),
            _ => None,
        }
    }
}

/// Share of the context window that starts a compaction under the cloud profile.
pub const CLOUD_COMPACTION_THRESHOLD: f32 = 0.90;
/// Recent context kept word for word under the cloud profile, in tokens.
pub const CLOUD_KEEP_RECENT_TOKENS: u32 = 20_000;
/// Soft length target of a summary under the cloud profile, in tokens.
pub const CLOUD_SUMMARY_TARGET: u32 = 2_000;

/// The compaction values one model runs with, after the profile and the explicit keys are resolved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionTuning {
    /// The profile in force. Never `Auto`.
    pub profile: CompactionProfile,
    /// `oxa.compaction.softTokens` when set: an absolute compaction point. It wins over `threshold`.
    pub soft_tokens: Option<u32>,
    /// Share of the window that starts a compaction when `soft_tokens` is unset. `None` = the
    /// engine's own point.
    pub threshold: Option<f32>,
    /// `compaction.keepRecentTokens` in force.
    pub keep_recent_tokens: u32,
    /// `oxa.compaction.summaryTarget` in force, in tokens; 0 = no target.
    pub summary_target: u32,
}

impl Settings {
    /// `compaction.profile` as set by a layer; `None` = unset or not a profile name.
    pub fn compaction_profile_setting(&self) -> Option<CompactionProfile> {
        self.string(CompactionProfile::SETTING)
            .as_deref()
            .and_then(CompactionProfile::parse)
    }

    /// The values a model runs with. `cloud_model` says whether the model is a cloud one; `auto`
    /// follows it. Under the cloud profile a key set explicitly in any layer wins over the profile.
    pub fn compaction_tuning(&self, cloud_model: bool) -> CompactionTuning {
        let cloud = match self.compaction_profile_setting().unwrap_or_default() {
            CompactionProfile::Auto => cloud_model,
            CompactionProfile::Local => false,
            CompactionProfile::Cloud => true,
        };
        let soft_tokens = self.compaction().soft_tokens;
        if !cloud {
            return CompactionTuning {
                profile: CompactionProfile::Local,
                soft_tokens,
                threshold: None,
                keep_recent_tokens: self.compaction().keep_recent_tokens,
                summary_target: self.summary_target(),
            };
        }
        CompactionTuning {
            profile: CompactionProfile::Cloud,
            soft_tokens,
            threshold: soft_tokens.is_none().then_some(CLOUD_COMPACTION_THRESHOLD),
            keep_recent_tokens: self.explicit_u32("compaction.keepRecentTokens", CLOUD_KEEP_RECENT_TOKENS),
            summary_target: self.explicit_u32("oxa.compaction.summaryTarget", CLOUD_SUMMARY_TARGET),
        }
    }

    /// `key` as a layer or an override sets it, else `default`.
    fn explicit_u32(&self, key: &str, default: u32) -> u32 {
        if self.is_explicit(key) {
            self.u32(key, default)
        } else {
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;
    use serde_json::json;

    #[test]
    fn project_cannot_raise_or_disable_the_image_limit() {
        let settings = Settings::from_values(
            json!({"oxa": {"read": {"maxImageBytes": 1024}}}),
            json!({"oxa": {"read": {"maxImageBytes": 0}}}),
        );
        assert_eq!(settings.read_max_image_bytes(), 1024);

        let default = Settings::from_values(json!({}), json!({"oxa": {"read": {"maxImageBytes": 0}}}));
        assert_eq!(default.read_max_image_bytes(), READ_MAX_IMAGE_BYTES_DEFAULT);
    }

    #[test]
    fn cloud_profile_keys_have_defaults() {
        let def = schema::find_key(CompactionProfile::SETTING).expect("compaction.profile in schema");
        assert_eq!(def.default, "\"auto\"");
        let none = Settings::from_values(json!({}), json!({}));
        assert_eq!(none.compaction_profile_setting(), None);
        let cloud = none.compaction_tuning(true);
        assert_eq!(cloud.profile, CompactionProfile::Cloud);
        assert_eq!(cloud.threshold, Some(CLOUD_COMPACTION_THRESHOLD));
        assert_eq!(cloud.keep_recent_tokens, 20_000);
        assert_eq!(cloud.summary_target, 2_000);
        assert_eq!(cloud.soft_tokens, None);
        let local = none.compaction_tuning(false);
        assert_eq!(local.profile, CompactionProfile::Local);
        assert_eq!(local.threshold, None);
        assert_eq!(local.keep_recent_tokens, 4_000);
        assert_eq!(local.summary_target, SUMMARY_TARGET_DEFAULT);
    }

    #[test]
    fn auto_picks_cloud_for_a_cloud_model() {
        let none = Settings::from_values(json!({}), json!({}));
        assert_eq!(none.compaction_tuning(true).profile, CompactionProfile::Cloud);
        assert_eq!(none.compaction_tuning(false).profile, CompactionProfile::Local);
        let local = Settings::from_values(json!({"compaction": {"profile": "local"}}), json!({}));
        assert_eq!(local.compaction_tuning(true).profile, CompactionProfile::Local);
        let cloud = Settings::from_values(json!({"compaction": {"profile": "cloud"}}), json!({}));
        assert_eq!(cloud.compaction_tuning(false).profile, CompactionProfile::Cloud);
        assert_eq!(cloud.compaction_profile_setting(), Some(CompactionProfile::Cloud));
    }

    #[test]
    fn explicit_keys_win_over_the_profile() {
        let user = Settings::from_values(
            json!({
                "compaction": {"profile": "cloud", "keepRecentTokens": 500},
                "oxa": {"compaction": {"summaryTarget": 0, "softTokens": 90000}}
            }),
            json!({}),
        );
        let tuning = user.compaction_tuning(true);
        assert_eq!(tuning.keep_recent_tokens, 500);
        assert_eq!(tuning.summary_target, 0);
        assert_eq!(tuning.soft_tokens, Some(90000));
        assert_eq!(tuning.threshold, None);

        // A project layer counts as explicit too.
        let project = Settings::from_values(
            json!({"compaction": {"profile": "cloud"}}),
            json!({"compaction": {"keepRecentTokens": 7}}),
        );
        assert_eq!(project.compaction_tuning(false).keep_recent_tokens, 7);
        assert_eq!(
            project.compaction_tuning(false).summary_target,
            CLOUD_SUMMARY_TARGET
        );
    }

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

    /// The background-job cap comes from the owner. Zero is treated as one job.
    #[test]
    fn jobs_limit_from_setting() {
        let wait = Settings::from_values(json!({"oxa": {"bash": {"waitCap": 90}}}), json!({}));
        assert_eq!(wait.bash_wait_cap(), 90);
        assert_eq!(
            Settings::from_values(json!({}), json!({})).bash_wait_cap(),
            BASH_WAIT_CAP_DEFAULT
        );
        let project = json!({"oxa": {"bash": {"maxJobs": 32}}});
        let set = Settings::from_values(json!({"oxa": {"bash": {"maxJobs": 3}}}), project.clone());
        assert_eq!(set.bash_max_jobs(), 3);
        let zero = Settings::from_values(json!({"oxa": {"bash": {"maxJobs": 0}}}), json!({}));
        assert_eq!(zero.bash_max_jobs(), 1);
        let unset = Settings::from_values(json!({}), project);
        assert_eq!(unset.bash_max_jobs(), 16);
    }
}
