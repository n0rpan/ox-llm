//! The hook set that offers `web_search` (read once per prefix epoch, W3) and the
//! `/web-search` status command.
use crate::brave::{Brave, BraveOpts, BraveShared, Endpoint, FALLBACK_NOTICE};
use crate::key::{load_key, KeyInputs, KeyOrigin, KeyStatus};
use crate::tool::{WebSearchTool, SOURCE};
use oxa_proto::{codes, CommandSpec, HookPoint, HookReply, Hooks, Level, OxaError, Source, Tool, Ui, Value};
use std::sync::Arc;

/// The status command (only listed while a key is configured).
pub const STATUS_COMMAND: &str = "web-search";

/// `OXA_WEB_SEARCH` switches the tool off with `0`, `false`, `no` or `off` (any case).
pub fn disabled_by_env(v: Option<&str>) -> bool {
    v.is_some_and(|v| {
        let v = v.trim();
        v == "0" || ["false", "no", "off"].iter().any(|f| v.eq_ignore_ascii_case(f))
    })
}

/// Whether the hook set is built at all: not with `--no-web-search`, `OXA_WEB_SEARCH=0`,
/// `oxa.webSearch.enabled: false` or `oxa.webSearch.provider: "none"`.
pub fn enabled(
    flag_disabled: bool,
    env_switch: Option<&str>,
    setting_enabled: bool,
    provider: Option<&str>,
) -> bool {
    !flag_disabled
        && !disabled_by_env(env_switch)
        && setting_enabled
        && provider.unwrap_or("brave") == "brave"
}

/// What the hook set is built from.
#[derive(Debug, Clone)]
pub struct WebConfig {
    pub key: KeyInputs,
    pub opts: BraveOpts,
}

impl WebConfig {
    /// The configuration from the environment snapshot (`get` = `Env::get`), the agent
    /// directory and `oxa.webSearch.maxTokens`. Returns the warnings to show at startup
    /// (a non-loopback `OXA_BRAVE_BASE_URL` is ignored).
    pub fn from_env(
        get: &dyn Fn(&str) -> Option<String>,
        agent_dir: &std::path::Path,
        max_tokens: Option<u32>,
    ) -> (WebConfig, Vec<String>) {
        let mut warnings = Vec::new();
        let isolation = get(crate::ENV_TEST_ISOLATION).is_some_and(|v| v.trim() == "1");
        let mut opts = BraveOpts {
            isolation,
            max_tokens: max_tokens.unwrap_or(crate::DEFAULT_MAX_TOKENS),
            ..BraveOpts::default()
        };
        if let Some(url) = get(crate::ENV_BASE_URL).filter(|u| !u.trim().is_empty()) {
            let loopback = (url.starts_with("http://") || url.starts_with("https://"))
                && oxa_proto::url_host(&url).is_some_and(|h| oxa_proto::is_loopback_host(&h));
            if loopback {
                opts.base_url = url;
            } else {
                warnings.push(format!(
                    "{} ignored: only a loopback URL is accepted (tests); web_search uses the real host",
                    crate::ENV_BASE_URL
                ));
            }
        }
        let key = KeyInputs {
            env_key: get(crate::ENV_KEY).and_then(|k| crate::ApiKey::new(&k)),
            agent_dir: agent_dir.to_path_buf(),
            isolation,
            real_home: crate::real_home(),
        };
        (WebConfig { key, opts }, warnings)
    }
}

/// `web_search` as a hook set. Each [`Hooks::tools`] call (once per epoch) loads the key
/// again: a key added mid-session is used from the next epoch, a removed one stops the tool
/// at the next epoch. Offline, no tool is declared. The fallback decision is shared by every
/// provider instance of the process.
pub struct WebSearchHooks {
    cfg: WebConfig,
    shared: Arc<BraveShared>,
}

impl std::fmt::Debug for WebSearchHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSearchHooks").field("cfg", &self.cfg).finish()
    }
}

impl WebSearchHooks {
    pub fn new(cfg: WebConfig) -> WebSearchHooks {
        WebSearchHooks {
            cfg,
            shared: BraveShared::new(),
        }
    }

    /// The state shared by the provider instances (tests read the request count).
    pub fn shared(&self) -> &Arc<BraveShared> {
        &self.shared
    }

    /// The startup notice when the key file is refused (problem shape; never the key).
    pub fn startup_notice(&self) -> Option<String> {
        match load_key(&self.cfg.key) {
            KeyStatus::Refused(text) => Some(text),
            _ => None,
        }
    }

    /// The `/web-search` lines.
    pub fn status_lines(&self) -> Vec<String> {
        let status = load_key(&self.cfg.key);
        let mut lines = Vec::new();
        let on = match &status {
            KeyStatus::Found(_, KeyOrigin::Env) => format!("key from {}", crate::ENV_KEY),
            KeyStatus::Found(_, KeyOrigin::File) => {
                format!(
                    "key from {}",
                    self.cfg.key.agent_dir.join(crate::KEY_FILE).display()
                )
            }
            KeyStatus::Missing => "no key configured".to_string(),
            KeyStatus::Refused(_) => "key file refused".to_string(),
        };
        let state = if oxa_proto::offline() {
            "off (offline mode)"
        } else if status.key().is_some() {
            "on"
        } else {
            "off"
        };
        lines.push(format!("web_search: {state} (Brave Search, {on})"));
        lines.push(match self.shared.endpoint() {
            Endpoint::LlmContext => "endpoint: LLM Context".to_string(),
            Endpoint::WebSearch => format!("endpoint: web search ({FALLBACK_NOTICE})"),
        });
        if let KeyStatus::Refused(text) = status {
            lines.push(text);
        }
        lines.push(
            "A key added or removed takes effect at the next prefix epoch (/reload or a new session)."
                .to_string(),
        );
        lines
    }

    fn tool(&self) -> Option<Arc<dyn Tool>> {
        if oxa_proto::offline() {
            return None;
        }
        let key = match load_key(&self.cfg.key) {
            KeyStatus::Found(k, _) => k,
            _ => return None,
        };
        let brave = Brave::with_shared(key, self.cfg.opts.clone(), self.shared.clone());
        Some(Arc::new(WebSearchTool::new(Arc::new(brave))))
    }
}

impl Hooks for WebSearchHooks {
    fn fire(&self, _p: HookPoint, _payload: &Value, _ui: &mut dyn Ui) -> HookReply {
        HookReply::Continue
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tool().into_iter().collect()
    }
    fn commands(&self) -> Vec<CommandSpec> {
        if load_key(&self.cfg.key).key().is_none() {
            return Vec::new();
        }
        vec![CommandSpec {
            name: STATUS_COMMAND.to_string(),
            description: "Show the web_search status (provider, key source, endpoint)".to_string(),
            argument_hint: None,
            source: Source::Extension(SOURCE.to_string()),
        }]
    }
    fn run_command(&self, name: &str, _args: &str, ui: &mut dyn Ui) -> Result<(), OxaError> {
        if name != STATUS_COMMAND {
            return Err(OxaError::new(
                codes::NOT_FOUND,
                format!("unknown command: /{name}"),
            ));
        }
        ui.notify(&self.status_lines().join("\n"), Level::Info);
        Ok(())
    }
    fn prompt_sections(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    fn name(&self) -> &str {
        SOURCE
    }
    fn wants(&self, _p: HookPoint) -> bool {
        false
    }
}
