//! `oxa.profile` (OXA-LOCAL-FIRST sec. 6) and the T5 `oxa.thinking.policy`: typed access and
//! the `OXA_PROFILE` environment override.
//!
//! Precedence of the profile: `OXA_PROFILE` > project settings > user settings >
//! [`Profile::DEFAULT`]. The environment is snapshotted by `oxa-cli` (P56: only
//! [`Env::from_process`] reads the process environment) and handed over once with
//! [`set_env_overrides`]; every later [`Settings::load`] (also `/reload`) sees it. Tests that
//! never call it see the settings alone.
use crate::{Diagnostic, Env, Settings};
use oxa_proto::{Level, Profile};
use std::sync::RwLock;

/// The process-wide `OXA_PROFILE` value, as validated by [`set_env_overrides`].
static ENV_PROFILE: RwLock<Option<Profile>> = RwLock::new(None);

/// Record the profile override of `env` for this process (`None` when `OXA_PROFILE` is unset
/// or empty). A value that is neither `pi` nor `local` is ignored and returned as a warning
/// naming the two values; the settings then decide.
pub fn set_env_overrides(env: &Env) -> Vec<Diagnostic> {
    let mut diags = Vec::new();
    let value = env.get(Profile::ENV).and_then(|v| match Profile::parse(v) {
        Some(p) => Some(p),
        None => {
            diags.push(Diagnostic {
                level: Level::Warning,
                source: Profile::ENV.to_string(),
                message: format!("{}; the settings decide", Profile::invalid_text(v)),
            });
            None
        }
    });
    if let Ok(mut slot) = ENV_PROFILE.write() {
        *slot = value;
    }
    diags
}

/// The `OXA_PROFILE` override recorded by [`set_env_overrides`].
pub fn env_profile() -> Option<Profile> {
    ENV_PROFILE.read().ok().and_then(|g| *g)
}

/// T5 `oxa.thinking.policy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingPolicy {
    /// pi: the session's thinking level applies to every request.
    Fixed,
    /// Thinking on for the first reply after a user prompt, off for every other request; the
    /// level `off` always wins.
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
    /// The policy when no layer sets one: `fixed` under `pi`, `auto` under `local`.
    pub fn default_for(profile: Profile) -> ThinkingPolicy {
        match profile {
            Profile::Pi => ThinkingPolicy::Fixed,
            Profile::Local => ThinkingPolicy::Auto,
        }
    }
}

/// Profile `local`: soft length target of a summary, in tokens.
pub const SUMMARY_TARGET_LOCAL: u32 = 700;
/// Profile `local`: `bash` timeout in seconds when the call sets none.
pub const BASH_TIMEOUT_LOCAL: u32 = 120;

/// The profile from `env` (already validated) over the merged settings.
pub fn resolve_profile(env: Option<Profile>, settings: &Settings) -> Profile {
    env.or_else(|| {
        settings
            .string(Profile::SETTING)
            .as_deref()
            .and_then(Profile::parse)
    })
    .unwrap_or(Profile::DEFAULT)
}

impl Settings {
    /// `oxa.profile` with the `OXA_PROFILE` override. Read once per prefix epoch (the core
    /// stores the result with the epoch; never call this per request).
    pub fn profile(&self) -> Profile {
        resolve_profile(env_profile(), self)
    }
    /// `oxa.thinking.policy` as set by a layer; `None` = unset (the profile decides).
    pub fn thinking_policy_setting(&self) -> Option<ThinkingPolicy> {
        self.string(ThinkingPolicy::SETTING)
            .as_deref()
            .and_then(ThinkingPolicy::parse)
    }
    /// The policy in force for an epoch of `profile`: an explicit setting wins in both profiles.
    pub fn thinking_policy(&self, profile: Profile) -> ThinkingPolicy {
        self.thinking_policy_setting()
            .unwrap_or_else(|| ThinkingPolicy::default_for(profile))
    }

    /// A `null`-default bool that the profile decides when no layer sets it: off under `pi`,
    /// on under `local`.
    fn profile_bool(&self, key: &str, profile: Profile) -> bool {
        self.bool(key, profile == Profile::Local)
    }
    /// `oxa.diet.output` (V020-OPTIMIZE #1): the output rules of the preamble.
    pub fn diet_output(&self, profile: Profile) -> bool {
        self.profile_bool("oxa.diet.output", profile)
    }
    /// `oxa.diet.tools` (V020-OPTIMIZE #2): lossless shaping of tool results.
    pub fn diet_tools(&self, profile: Profile) -> bool {
        self.profile_bool("oxa.diet.tools", profile)
    }
    /// `oxa.compaction.idleSummary` (V020-OPTIMIZE #7): the summary is computed ahead in idle
    /// time.
    pub fn idle_summary(&self, profile: Profile) -> bool {
        self.profile_bool("oxa.compaction.idleSummary", profile)
    }
    /// `oxa.compaction.turnStart`: compact before a prompt is sent when the context is already
    /// above the compaction point (for example after an aborted turn). Unset = off under `pi`,
    /// on under `local`.
    pub fn turn_start_compaction(&self, profile: Profile) -> bool {
        self.profile_bool("oxa.compaction.turnStart", profile)
    }
    /// `oxa.compaction.midRun`: compact between tool steps of a run when the context passes the
    /// compaction point. Unset = off under `pi`, on under `local`.
    pub fn mid_run_compaction(&self, profile: Profile) -> bool {
        self.profile_bool("oxa.compaction.midRun", profile)
    }
    /// `oxa.prefill.paste` (V020-OPTIMIZE #13).
    pub fn prefill_paste(&self, profile: Profile) -> bool {
        self.profile_bool("oxa.prefill.paste", profile)
    }
    /// `oxa.prefill.tools` (OXA-LOCAL-FIRST F1).
    pub fn prefill_tools(&self, profile: Profile) -> bool {
        self.profile_bool("oxa.prefill.tools", profile)
    }
    /// `oxa.compaction.summaryTarget` in tokens (V020-OPTIMIZE #7); 0 = no target. Unset = 0
    /// under `pi`, [`SUMMARY_TARGET_LOCAL`] under `local`.
    pub fn summary_target(&self, profile: Profile) -> u32 {
        let default = if profile == Profile::Local {
            SUMMARY_TARGET_LOCAL
        } else {
            0
        };
        self.u32("oxa.compaction.summaryTarget", default)
    }
    /// `oxa.bash.defaultTimeout` in seconds (F5); 0 = none. Unset = 0 under `pi`,
    /// [`BASH_TIMEOUT_LOCAL`] under `local`.
    pub fn bash_default_timeout(&self, profile: Profile) -> u32 {
        let default = if profile == Profile::Local {
            BASH_TIMEOUT_LOCAL
        } else {
            0
        };
        self.u32("oxa.bash.defaultTimeout", default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;
    use serde_json::json;

    #[test]
    fn schema_default_is_the_constant() {
        let def = schema().iter().find(|d| d.key == Profile::SETTING).unwrap();
        assert_eq!(def.default, format!("\"{}\"", Profile::DEFAULT.as_str()));
        let pol = schema()
            .iter()
            .find(|d| d.key == ThinkingPolicy::SETTING)
            .unwrap();
        assert_eq!(pol.default, "null");
    }

    #[test]
    fn precedence_env_over_project_over_user() {
        let none = Settings::from_values(json!({}), json!({}));
        assert_eq!(resolve_profile(None, &none), Profile::DEFAULT);
        let user = Settings::from_values(json!({"oxa": {"profile": "local"}}), json!({}));
        assert_eq!(resolve_profile(None, &user), Profile::Local);
        let project = Settings::from_values(
            json!({"oxa": {"profile": "local"}}),
            json!({"oxa": {"profile": "pi"}}),
        );
        assert_eq!(resolve_profile(None, &project), Profile::Pi);
        assert_eq!(resolve_profile(Some(Profile::Local), &project), Profile::Local);
        assert_eq!(resolve_profile(Some(Profile::Pi), &user), Profile::Pi);
    }

    #[test]
    fn a_bad_value_is_a_diagnostic_naming_both_values() {
        let s = Settings::from_values(json!({"oxa": {"profile": "fast"}}), json!({}));
        assert_eq!(resolve_profile(None, &s), Profile::DEFAULT);
        let d = s
            .diagnostics()
            .iter()
            .find(|d| d.message.contains("oxa.profile"))
            .expect("a diagnostic for oxa.profile");
        assert!(d.message.contains("\"pi\", \"local\""), "{}", d.message);
        // A bad project value falls back to the user's.
        let s = Settings::from_values(
            json!({"oxa": {"profile": "local"}}),
            json!({"oxa": {"profile": 3}}),
        );
        assert_eq!(resolve_profile(None, &s), Profile::Local);
    }

    #[test]
    fn env_override_validates() {
        // The only test in this crate that touches the process-wide override; it restores it.
        let d = set_env_overrides(&Env::from_pairs(&[("OXA_PROFILE", "nope")]));
        assert_eq!(d.len(), 1);
        assert!(d[0].message.contains("\"pi\" or \"local\""), "{}", d[0].message);
        assert_eq!(env_profile(), None);
        assert!(set_env_overrides(&Env::from_pairs(&[("OXA_PROFILE", "local")])).is_empty());
        assert_eq!(env_profile(), Some(Profile::Local));
        assert_eq!(
            Settings::from_values(json!({}), json!({})).profile(),
            Profile::Local
        );
        assert!(set_env_overrides(&Env::from_pairs(&[])).is_empty());
        assert_eq!(env_profile(), None);
    }

    #[test]
    fn thinking_policy_defaults_by_profile_and_setting_wins() {
        let none = Settings::from_values(json!({}), json!({}));
        assert_eq!(none.thinking_policy(Profile::Pi), ThinkingPolicy::Fixed);
        assert_eq!(none.thinking_policy(Profile::Local), ThinkingPolicy::Auto);
        let auto = Settings::from_values(json!({"oxa": {"thinking": {"policy": "auto"}}}), json!({}));
        assert_eq!(auto.thinking_policy(Profile::Pi), ThinkingPolicy::Auto);
        let fixed = Settings::from_values(json!({"oxa": {"thinking": {"policy": "fixed"}}}), json!({}));
        assert_eq!(fixed.thinking_policy(Profile::Local), ThinkingPolicy::Fixed);
        let bad = Settings::from_values(json!({"oxa": {"thinking": {"policy": "on"}}}), json!({}));
        assert_eq!(bad.thinking_policy(Profile::Local), ThinkingPolicy::Auto);
        assert!(bad
            .diagnostics()
            .iter()
            .any(|d| d.message.contains("\"fixed\", \"auto\"")));
    }
}
