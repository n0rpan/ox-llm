//! B6: the OXA_* environment table.
// ported from pi packages/coding-agent/docs/environment-variables.md

/// One documented environment variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvVar {
    pub name: &'static str,
    /// pi's name, when this is a renamed `PI_*` variable.
    pub pi_name: Option<&'static str>,
    pub description: &'static str,
    /// `false` = accepted and ignored (verdict O), with a warning when set.
    pub supported: bool,
}

/// Names oxa reads or sets. Consumers look names up here instead of spelling them.
pub(crate) const ENGINE_URL: &str = "OXA_ENGINE_URL";
pub(crate) const HOME: &str = "HOME";
pub(crate) const SESSION_DIR: &str = "OXA_SESSION_DIR";
pub(crate) const SYSTEM_SKILLS_DIR: &str = "OXA_SYSTEM_SKILLS_DIR";
pub(crate) const MODELS_DEFAULT_KEY: &str = "default";

/// Every variable of pi's environment-variables.md under its `OXA_*` name, plus
/// `OXA_ENGINE_URL`. Variables that pi does not rename (`AI_AGENT`, `VISUAL`, `EDITOR`,
/// `HTTP_PROXY`, `HTTPS_PROXY`) keep their own name and have no `pi_name`.
/// Filled by B6; consumers look names up here instead of spelling them.
pub const ENV_TABLE: &[EnvVar] = &[
    // Process markers and the endpoint.
    EnvVar {
        name: "AI_AGENT",
        pi_name: None,
        description: "process marker for child processes; set to \"oxa\"",
        supported: true,
    },
    EnvVar {
        name: "OXA_CODING_AGENT",
        pi_name: Some("PI_CODING_AGENT"),
        description: "process marker, \"true\" for child processes",
        supported: true,
    },
    EnvVar {
        name: "OXA_ENGINE_URL",
        pi_name: None,
        description: "HTTP endpoint used only with both native switches explicitly disabled; compatibility default http://127.0.0.1:8092",
        supported: true,
    },
    // Shell tool session environment, set per run (never on the wire, W3).
    EnvVar {
        name: "OXA_SESSION_ID",
        pi_name: Some("PI_SESSION_ID"),
        description: "current session id, for bash tool commands",
        supported: true,
    },
    EnvVar {
        name: "OXA_SESSION_FILE",
        pi_name: Some("PI_SESSION_FILE"),
        description: "absolute path of the session file; unset when ephemeral",
        supported: true,
    },
    EnvVar {
        name: "OXA_PROVIDER",
        pi_name: Some("PI_PROVIDER"),
        description: "selected provider; always \"oxe\"",
        supported: true,
    },
    EnvVar {
        name: "OXA_MODEL",
        pi_name: Some("PI_MODEL"),
        description: "selected model id",
        supported: true,
    },
    EnvVar {
        name: "OXA_REASONING_LEVEL",
        pi_name: Some("PI_REASONING_LEVEL"),
        description: "effective thinking level: off, minimal, low, medium, high, xhigh or max",
        supported: true,
    },
    // Configuration and directories.
    EnvVar {
        name: "OXA_AGENT_DIR",
        pi_name: Some("PI_CODING_AGENT_DIR"),
        description: "agent directory; default ~/.oxa/agent; ~ is expanded",
        supported: true,
    },
    EnvVar {
        name: "OXA_SYSTEM_SKILLS_DIR",
        pi_name: None,
        description: "system skills directory read last; default /usr/share/omarchy/default/agents/skills; empty = none",
        supported: true,
    },
    EnvVar {
        name: "OXA_TEST_ISOLATION",
        pi_name: None,
        description: "test processes only (set by the test harness): enables the test hooks below; no host directory is read as a fallback (system skills)",
        supported: true,
    },
    EnvVar {
        name: "OXA_TEST_PANIC",
        pi_name: None,
        description: "test processes only, with OXA_TEST_ISOLATION=1: host_function = extensions get a host function that panics (panic isolation test)",
        supported: true,
    },
    EnvVar {
        name: "OXA_SESSION_DIR",
        pi_name: Some("PI_CODING_AGENT_SESSION_DIR"),
        description: "session storage; overridden by --session-dir",
        supported: true,
    },
    EnvVar {
        name: "OXA_PACKAGE_DIR",
        pi_name: Some("PI_PACKAGE_DIR"),
        description: "package directory override, for Nix or Guix store paths",
        supported: true,
    },
    EnvVar {
        name: "OXA_ALLOW_HOME_CWD",
        pi_name: None,
        description: "allow a session in the home directory or above it (1/true/yes/on), as oxa.allowHomeCwd; without it oxa refuses there (print, json, rpc) or asks (interactive)",
        supported: true,
    },
    EnvVar {
        name: "OXA_OFFLINE",
        pi_name: Some("PI_OFFLINE"),
        description: "hard offline switch, as --offline (1/true/yes/on): non-local connections refused, bash has no network, web_search off",
        supported: true,
    },
    // O-169: the web_search tool (oxa-web); oxa-only, pi has no built-in search.
    EnvVar {
        name: "BRAVE_SEARCH_API_KEY",
        pi_name: None,
        description: "Brave Search API key for web_search (wins over <agent dir>/brave-search.key); never passed to child processes",
        supported: true,
    },
    EnvVar {
        name: "OXA_WEB_SEARCH",
        pi_name: None,
        description: "0/false/no/off switches the web_search tool off, 1/true/yes/on says on (as --no-web-search)",
        supported: true,
    },
    EnvVar {
        name: "OXA_SKIP_VERSION_CHECK",
        pi_name: Some("PI_SKIP_VERSION_CHECK"),
        description: "accepted and ignored: oxa has no version check",
        supported: false,
    },
    EnvVar {
        name: "OXA_SELF_UPDATE_COMMAND",
        pi_name: Some("PI_SELF_UPDATE_COMMAND"),
        description: "accepted and ignored: oxa has no self-update",
        supported: false,
    },
    EnvVar {
        name: "OXA_TELEMETRY",
        pi_name: Some("PI_TELEMETRY"),
        description: "accepted and ignored: oxa has no telemetry",
        supported: false,
    },
    EnvVar {
        name: "OXA_CODEMODE_MAX_WORKERS",
        pi_name: Some("PI_CODEMODE_MAX_WORKERS"),
        description: "codemode workers alive at once in the process; default 2",
        supported: true,
    },
    EnvVar {
        name: "OXA_CACHE_RETENTION",
        pi_name: Some("PI_CACHE_RETENTION"),
        description: "accepted and ignored: the engine owns its cache",
        supported: false,
    },
    EnvVar {
        name: "OXA_SHARE_VIEWER_URL",
        pi_name: Some("PI_SHARE_VIEWER_URL"),
        description: "accepted and ignored: oxa does no uploads",
        supported: false,
    },
    EnvVar {
        name: "OXA_RADIUS_GATEWAY",
        pi_name: Some("PI_RADIUS_GATEWAY"),
        description: "accepted and ignored: oxa does no uploads",
        supported: false,
    },
    // Terminal and TUI.
    EnvVar {
        name: "OXA_CLEAR_ON_SHRINK",
        pi_name: Some("PI_CLEAR_ON_SHRINK"),
        description: "1/true/yes/on clears empty rows when rendered content shrinks",
        supported: true,
    },
    EnvVar {
        name: "OXA_SKIN",
        pi_name: None,
        description: "pi, oxa or ascii (ASCII glyphs only): the look of the interactive UI for this run (over the ui.skin setting)",
        supported: true,
    },
    EnvVar {
        name: "OXA_REDUCED_MOTION",
        pi_name: None,
        description: "1/true/yes/on switches the animations of the oxa skin off, 0/false/no/off turns them back on for one run (as the ui.reducedMotion setting)",
        supported: true,
    },
    EnvVar {
        name: "OXA_CLIPBOARD_OSC52",
        pi_name: Some("PI_CLIPBOARD_OSC52"),
        description: "1/true/yes/on also copies through OSC 52",
        supported: true,
    },
    // Standard names, not renamed by pi.
    EnvVar {
        name: "VISUAL",
        pi_name: None,
        description: "external editor fallback when externalEditor is unset",
        supported: true,
    },
    EnvVar {
        name: "EDITOR",
        pi_name: None,
        description: "external editor fallback when VISUAL is unset",
        supported: true,
    },
    EnvVar {
        name: "HTTP_PROXY",
        pi_name: None,
        description: "proxy for MCP HTTP requests only",
        supported: true,
    },
    EnvVar {
        name: "HTTPS_PROXY",
        pi_name: None,
        description: "proxy for MCP HTTPS requests only",
        supported: true,
    },
];

/// The documented words that turn a user-facing `OXA_*` boolean on, in any case.
pub const TRUE_WORDS: [&str; 4] = ["1", "true", "yes", "on"];
/// The documented words that turn a user-facing `OXA_*` boolean off, in any case.
pub const FALSE_WORDS: [&str; 4] = ["0", "false", "no", "off"];

/// One user-facing boolean environment variable: the same eight words everywhere
/// (`1`/`true`/`yes`/`on` and `0`/`false`/`no`/`off`, any case, surrounding space ignored).
///
/// Unset and empty are [`EnvFlag::Unset`]; any other word is [`EnvFlag::Invalid`]. Both leave
/// the caller's own default in place ([`EnvFlag::or`]), so a variable that is not set for a
/// caller never changes that caller's behaviour. Internal test markers (`OXA_TEST_*`) are not
/// parsed here: they have their own exact spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvFlag {
    On,
    Off,
    /// Unset or empty: the caller's default applies.
    Unset,
    /// Set to a word outside [`TRUE_WORDS`] / [`FALSE_WORDS`]: the caller's default applies.
    Invalid,
}

impl EnvFlag {
    /// Parse a raw environment value. `None` (unset) and an empty or blank value are
    /// [`EnvFlag::Unset`]; the eight documented words are matched case-insensitively.
    pub fn parse(value: Option<&str>) -> EnvFlag {
        let Some(v) = value.map(str::trim) else {
            return EnvFlag::Unset;
        };
        if v.is_empty() {
            return EnvFlag::Unset;
        }
        if TRUE_WORDS.iter().any(|w| v.eq_ignore_ascii_case(w)) {
            EnvFlag::On
        } else if FALSE_WORDS.iter().any(|w| v.eq_ignore_ascii_case(w)) {
            EnvFlag::Off
        } else {
            EnvFlag::Invalid
        }
    }

    /// The effective boolean: `default` for [`EnvFlag::Unset`] and [`EnvFlag::Invalid`].
    pub fn or(self, default: bool) -> bool {
        match self {
            EnvFlag::On => true,
            EnvFlag::Off => false,
            EnvFlag::Unset | EnvFlag::Invalid => default,
        }
    }

    /// `true` when the value was set to a word outside the documented eight.
    pub fn is_invalid(self) -> bool {
        self == EnvFlag::Invalid
    }
}

/// [`EnvFlag::parse`] with the caller's default in one call.
pub fn env_flag(value: Option<&str>, default: bool) -> bool {
    EnvFlag::parse(value).or(default)
}

/// A snapshot of the environment. The only reader of the process environment in the workspace
/// is [`Env::from_process`], called once from `main`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Env {
    vars: Vec<(String, String)>,
}

impl Env {
    pub fn from_process() -> Env {
        Env {
            // `vars()` panics on a non-UTF-8 entry; such entries are skipped instead.
            vars: std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
        }
    }
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Env {
        Env {
            vars: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }
    /// The value of `name`; empty values count as unset.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }
    pub fn pairs(&self) -> &[(String, String)] {
        &self.vars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_eight_words_parse_in_any_case() {
        for word in TRUE_WORDS {
            assert_eq!(EnvFlag::parse(Some(word)), EnvFlag::On, "{word}");
            assert_eq!(
                EnvFlag::parse(Some(&word.to_ascii_uppercase())),
                EnvFlag::On,
                "{word} upper"
            );
        }
        for word in FALSE_WORDS {
            assert_eq!(EnvFlag::parse(Some(word)), EnvFlag::Off, "{word}");
            assert_eq!(
                EnvFlag::parse(Some(&word.to_ascii_uppercase())),
                EnvFlag::Off,
                "{word} upper"
            );
        }
        // Mixed case, as a user types it.
        assert_eq!(EnvFlag::parse(Some("True")), EnvFlag::On);
        assert_eq!(EnvFlag::parse(Some("oFf")), EnvFlag::Off);
        assert_eq!(EnvFlag::parse(Some("  yes  ")), EnvFlag::On);
    }

    #[test]
    fn unset_empty_and_invalid_keep_the_caller_default() {
        for v in [None, Some(""), Some("   ")] {
            assert_eq!(EnvFlag::parse(v), EnvFlag::Unset, "{v:?}");
        }
        for v in ["maybe", "2", "t", "y", "no-way"] {
            assert_eq!(EnvFlag::parse(Some(v)), EnvFlag::Invalid, "{v}");
            assert!(EnvFlag::parse(Some(v)).is_invalid());
        }
        // Both keep each caller's own default, in both directions.
        for v in [None, Some(""), Some("maybe")] {
            assert!(EnvFlag::parse(v).or(true));
            assert!(!EnvFlag::parse(v).or(false));
        }
        assert!(env_flag(Some("on"), false));
        assert!(!env_flag(Some("off"), true));
        assert!(env_flag(None, true) && !env_flag(None, false));
    }
}
