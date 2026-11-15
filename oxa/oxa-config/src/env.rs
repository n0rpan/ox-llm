//! B6: the OXA_* environment table. Owner: task B6. Signatures below are frozen; bodies are yours.
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
        description: "engine endpoint for the default models.json entry; default http://127.0.0.1:8092 (the oxc controller)",
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
        description: "allow a session in the home directory or above it (1, true, yes), as oxa.allowHomeCwd; without it oxa refuses there (print, json, rpc) or asks (interactive)",
        supported: true,
    },
    EnvVar {
        name: "OXA_OFFLINE",
        pi_name: Some("PI_OFFLINE"),
        description: "hard offline switch, as --offline (1, true, yes): non-local connections refused, bash has no network, web_search off",
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
        description: "0/false/no/off switches the web_search tool off (as --no-web-search)",
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
        description: "1 clears empty rows when rendered content shrinks",
        supported: true,
    },
    EnvVar {
        name: "OXA_SKIN",
        pi_name: None,
        description: "pi or oxa: the look of the interactive UI for this run (over the ui.skin setting)",
        supported: true,
    },
    EnvVar {
        name: "OXA_REDUCED_MOTION",
        pi_name: None,
        description: "1 switches the animations of the oxa skin off (as the ui.reducedMotion setting)",
        supported: true,
    },
    EnvVar {
        name: "OXA_CLIPBOARD_OSC52",
        pi_name: Some("PI_CLIPBOARD_OSC52"),
        description: "1 also copies through OSC 52",
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
