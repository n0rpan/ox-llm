//! B6: the OXA_* environment table.
// ported from pi packages/coding-agent/docs/environment-variables.md

/// Who sets an environment variable and who it is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvKind {
    /// The owner sets it; documented in `oxa/docs/pages/cli.md`.
    User,
    /// oxa sets it for the processes it starts (bash tool, extension tools, check commands);
    /// documented in `cli.md`.
    Set,
    /// Parent-to-child protocol between oxa processes, or a test hook. Not for users; listed
    /// in `cli.md` under "Internal".
    Internal,
}

/// One documented environment variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvVar {
    pub name: &'static str,
    /// pi's name, when this is a renamed `PI_*` variable.
    pub pi_name: Option<&'static str>,
    pub description: &'static str,
    /// `false` = accepted and ignored (verdict O): nothing reads it.
    pub supported: bool,
    pub kind: EnvKind,
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
    EnvVar {
        name: "OXA_LEAD_BOARD_LOG",
        pi_name: None,
        description: "owner-selected absolute path to a private local board sample log; missing or refused input stays unavailable",
        supported: true,
        kind: EnvKind::User,
    },
    // Process markers and the endpoint.
    EnvVar {
        name: "AI_AGENT",
        pi_name: None,
        description: "process marker for child processes; set to \"oxa\"",
        supported: true,
        kind: EnvKind::Set,
    },
    EnvVar {
        name: "OXA_CODING_AGENT",
        pi_name: Some("PI_CODING_AGENT"),
        description: "process marker, \"true\" for child processes",
        supported: true,
        kind: EnvKind::Set,
    },
    EnvVar {
        name: "OXA_ENGINE_URL",
        pi_name: None,
        description: "engine endpoint for the default models.json entry; default http://127.0.0.1:8092 (a local engine endpoint)",
        supported: true,
        kind: EnvKind::User,
    },
    // Shell tool session environment, set per run (never on the wire, W3).
    EnvVar {
        name: "OXA_SESSION_ID",
        pi_name: Some("PI_SESSION_ID"),
        description: "current session id, for bash tool commands",
        supported: true,
        kind: EnvKind::Set,
    },
    EnvVar {
        name: "OXA_SESSION_FILE",
        pi_name: Some("PI_SESSION_FILE"),
        description: "absolute path of the session file; unset when ephemeral",
        supported: true,
        kind: EnvKind::Set,
    },
    EnvVar {
        name: "OXA_PROVIDER",
        pi_name: Some("PI_PROVIDER"),
        description: "selected provider; always \"oxe\"",
        supported: true,
        kind: EnvKind::Set,
    },
    EnvVar {
        name: "OXA_MODEL",
        pi_name: Some("PI_MODEL"),
        description: "selected model id",
        supported: true,
        kind: EnvKind::Set,
    },
    EnvVar {
        name: "OXA_REASONING_LEVEL",
        pi_name: Some("PI_REASONING_LEVEL"),
        description: "effective thinking level: off, minimal, low, medium, high, xhigh or max",
        supported: true,
        kind: EnvKind::Set,
    },
    // Configuration and directories.
    EnvVar {
        name: "OXA_AGENT_DIR",
        pi_name: Some("PI_CODING_AGENT_DIR"),
        description: "agent directory; default ~/.oxa/agent; ~ is expanded",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_SYSTEM_SKILLS_DIR",
        pi_name: None,
        description: "system skills directory read last; default /usr/share/omarchy/default/agents/skills; empty = none",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_TEST_ISOLATION",
        pi_name: None,
        description: "test processes only (set by the test harness): enables the test hooks below; no host directory is read as a fallback (system skills)",
        supported: true,
        kind: EnvKind::Internal,
    },
    EnvVar {
        name: "OXA_TEST_PANIC",
        pi_name: None,
        description: "test processes only, with OXA_TEST_ISOLATION=1: host_function = extensions get a host function that panics (panic isolation test)",
        supported: true,
        kind: EnvKind::Internal,
    },
    EnvVar {
        name: "OXA_SESSION_DIR",
        pi_name: Some("PI_CODING_AGENT_SESSION_DIR"),
        description: "session storage; overridden by --session-dir",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_PACKAGE_DIR",
        pi_name: Some("PI_PACKAGE_DIR"),
        description: "accepted and ignored: oxa has no package store to relocate",
        supported: false,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_ALLOW_HOME_CWD",
        pi_name: None,
        description: "allow a session in the home directory or above it (1/true/yes/on), as oxa.allowHomeCwd; without it oxa refuses there (print, json, rpc) or asks (interactive)",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_OFFLINE",
        pi_name: Some("PI_OFFLINE"),
        description: "hard offline switch, as --offline (1/true/yes/on): non-local connections refused, bash has no network, web_search off",
        supported: true,
        kind: EnvKind::User,
    },
    // O-169: the web_search tool (oxa-web); oxa-only, pi has no built-in search.
    EnvVar {
        name: "BRAVE_SEARCH_API_KEY",
        pi_name: None,
        description: "Brave Search API key for web_search (wins over <agent dir>/brave-search.key); never passed to child processes",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_WEB_SEARCH",
        pi_name: None,
        description: "0/false/no/off switches the web_search tool off, 1/true/yes/on says on (as --no-web-search)",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_SKIP_VERSION_CHECK",
        pi_name: Some("PI_SKIP_VERSION_CHECK"),
        description: "accepted and ignored: oxa has no version check",
        supported: false,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_SELF_UPDATE_COMMAND",
        pi_name: Some("PI_SELF_UPDATE_COMMAND"),
        description: "accepted and ignored: oxa has no self-update",
        supported: false,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_TELEMETRY",
        pi_name: Some("PI_TELEMETRY"),
        description: "accepted and ignored: oxa has no telemetry",
        supported: false,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_CODEMODE_MAX_WORKERS",
        pi_name: Some("PI_CODEMODE_MAX_WORKERS"),
        description: "codemode workers alive at once in the process; default 2",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_CACHE_RETENTION",
        pi_name: Some("PI_CACHE_RETENTION"),
        description: "accepted and ignored: the engine owns its cache",
        supported: false,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_SHARE_VIEWER_URL",
        pi_name: Some("PI_SHARE_VIEWER_URL"),
        description: "accepted and ignored: oxa does no uploads",
        supported: false,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_RADIUS_GATEWAY",
        pi_name: Some("PI_RADIUS_GATEWAY"),
        description: "accepted and ignored: oxa does no uploads",
        supported: false,
        kind: EnvKind::User,
    },
    // Terminal and TUI.
    EnvVar {
        name: "OXA_CLEAR_ON_SHRINK",
        pi_name: Some("PI_CLEAR_ON_SHRINK"),
        description: "1/true/yes/on clears empty rows when rendered content shrinks",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_SKIN",
        pi_name: None,
        description: "pi, oxa or ascii (ASCII glyphs only): the look of the interactive UI for this run (over the ui.skin setting)",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_REDUCED_MOTION",
        pi_name: None,
        description: "1/true/yes/on switches the animations of the oxa skin off, 0/false/no/off turns them back on for one run (as the ui.reducedMotion setting)",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_CLIPBOARD_OSC52",
        pi_name: Some("PI_CLIPBOARD_OSC52"),
        description: "1/true/yes/on also copies through OSC 52",
        supported: true,
        kind: EnvKind::User,
    },
    // Standard names, not renamed by pi.
    EnvVar {
        name: "VISUAL",
        pi_name: None,
        description: "external editor fallback when externalEditor is unset",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "EDITOR",
        pi_name: None,
        description: "external editor fallback when VISUAL is unset",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "HTTP_PROXY",
        pi_name: None,
        description: "proxy for MCP HTTP requests only",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "HTTPS_PROXY",
        pi_name: None,
        description: "proxy for MCP HTTPS requests only",
        supported: true,
        kind: EnvKind::User,
    },
    // Names found by the source scan (`tests/env_names_documented.rs`).
    EnvVar {
        name: "OXA_ENV_NOTE",
        pi_name: None,
        description: "0/false/no/off or 1/true/yes/on forces the environment note off or on for this run; wins over the oxa.envNote setting",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "NO_COLOR",
        pi_name: None,
        description: "any non-empty value: no colour and no animation with the oxa skin; also set to 1 on the gh probes oxa runs",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_TRUST_PROJECT",
        pi_name: None,
        description: "1 or true is --approve for the project trust decision, for unattended runs; an explicit --no-approve still wins; removed from worker and work child environments",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_RPC_OWNER_PRESENT",
        pi_name: None,
        description: "1 tells an RPC session that an owner can answer questions, so the ask tool is offered; other values leave it off",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_LEAD_SITUATION",
        pi_name: None,
        description: "owner-selected absolute path to the runner's situation document for oxa lead; relative paths are refused, an unreadable or unprotected file stays unavailable",
        supported: true,
        kind: EnvKind::User,
    },
    EnvVar {
        name: "OXA_WORK_OUTPUT_DIR",
        pi_name: None,
        description: "oxa work sets it for done-check commands: the private output directory (also TMPDIR and CARGO_TARGET_DIR/target) when the task has one",
        supported: true,
        kind: EnvKind::Set,
    },
    EnvVar {
        name: "OXA_RUNNING_BIN",
        pi_name: None,
        description: "the sandbox sets it inside the bash sandbox to the absolute path of the running oxa binary, when that file is readable there",
        supported: true,
        kind: EnvKind::Set,
    },
    EnvVar {
        name: "OXA_TOOL_CALL_ID",
        pi_name: None,
        description: "oxa sets it in the environment of an extension tool call to the id of that call",
        supported: true,
        kind: EnvKind::Set,
    },
    EnvVar {
        name: "OXA_SUBAGENT_DEPTH",
        pi_name: None,
        description: "a parent oxa sets 1 in a subagent child; a value above 0 or a non-number marks the process as a child, which cannot start subagents; counts as an agent marker like AI_AGENT",
        supported: true,
        kind: EnvKind::Internal,
    },
    EnvVar {
        name: "OXA_SESSION_HINT",
        pi_name: None,
        description: "a parent oxa sets <parent session id>.s<n> in a subagent child; every chat request of the child names it; a malformed value is ignored with a notice",
        supported: true,
        kind: EnvKind::Internal,
    },
    EnvVar {
        name: "OXA_CHILD_SOFT_TOKENS",
        pi_name: None,
        description: "a parent oxa may set the soft compaction limit of a subagent child; overrides oxa.compaction.softTokens; a bad value is ignored with a notice",
        supported: true,
        kind: EnvKind::Internal,
    },
    EnvVar {
        name: "OXA_WORKER_DEPTH",
        pi_name: None,
        description: "oxa sets 1 in the environment of a lead worker session; dropped from the parent environment before it is set",
        supported: true,
        kind: EnvKind::Internal,
    },
    EnvVar {
        name: "OXA_WORK_MODEL_DELEGATED",
        pi_name: None,
        description: "oxa work sets 1 in a worker; a restored or project-selected model then never counts as chosen by the owner, while an actual model argument is still admitted by a protected Go key",
        supported: true,
        kind: EnvKind::Internal,
    },
    EnvVar {
        name: "OXA_WORKTREE_TRUST_SOURCE",
        pi_name: None,
        description: "oxa work sets the trusted source checkout for a managed worktree; honoured only in --mode json, and only when the checkout, the trust record and the approved resources verify",
        supported: true,
        kind: EnvKind::Internal,
    },
    EnvVar {
        name: "OXA_RUNNER_OWNS_GITHUB",
        pi_name: None,
        description: "oxa work sets 1 in a worker: the runner owns GitHub access, so the worker runs no gh and no authenticated GitHub HTTP",
        supported: true,
        kind: EnvKind::Internal,
    },
    EnvVar {
        name: "OXA_NATIVE_STEERING_FD",
        pi_name: None,
        description: "oxa work passes the inherited descriptor number of the steering lease to a native worker; the worker verifies the held lock before it accepts the assignment",
        supported: true,
        kind: EnvKind::Internal,
    },
    EnvVar {
        name: "OXA_BRAVE_BASE_URL",
        pi_name: None,
        description: "test hook: base URL of the Brave Search provider, honoured only for a loopback URL (and under OXA_TEST_ISOLATION)",
        supported: true,
        kind: EnvKind::Internal,
    },
    EnvVar {
        name: "OXE_API_KEY_FILE",
        pi_name: None,
        description: "oxa sets it for a local engine child it starts: the private file holding the child's bearer token",
        supported: true,
        kind: EnvKind::Internal,
    },
    EnvVar {
        name: "OXE_API_KEY",
        pi_name: None,
        description: "not read by oxa: a local engine profile that sets this name or OXE_API_KEY_FILE itself is refused",
        supported: true,
        kind: EnvKind::Internal,
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
